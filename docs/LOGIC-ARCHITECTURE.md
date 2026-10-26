# CodecTrans 로직(워크플로 그래프) 구조 점검

> 로직 탭의 그래프가 **어디에 저장**되고 **어떤 순서로 실행**되며, **어디를 확장**하면
> 새 기능이 되는지에 대한 지도. 2026-10-26 플러그인(`field`/`cmd`/`postCmd`) 도입 시 작성.

## 1. 저장소 (Persistence)

- **파일**: `base_dir()/library.json` — 서버 설정 디렉터리
  (`CT_CONFIG_DIR` env, QNAP에서는 컨테이너 볼륨으로 마운트됨).
  구조: `LibraryConfig` (`backend/src/main.rs`)
  - `logics: Vec<NamedLogic>` — **명명된 로직 블록** (로직 탭에서 생성/수정)
    각 로직 = `{ id, name, graph: WorkflowGraph }`
  - `folders: Vec<FolderRule>` — 폴더별 할당: `path → { logic_id?, target(일괄 기본), graph(레거시 인라인) }`
  - 전역 라이브러리 그래프 (`eff_graph`) — 폴더가 특정 로직을 지정하지 않은 파일의 폴백
  - 서버 세팅 (실패 보관 폴더, 자동 인코딩 기본값 등)

- **쓰기 규칙**: 모든 read-modify-write는 `LIB_CFG_LOCK`(프로세스 전역) 아래
  원자적 — 스캔/웹 API 동시 수정 안전. 파일은 tmp+rename으로 교체.

- `WorkflowGraph` = `{ nodes: Vec<GraphNode>, edges: Vec<Edge> }`
  - `GraphNode` = `{ id, kind: "start"|"condition"|"action", enabled, x, y, label?, condition?, action? }`
  - `Edge` = `{ from, to, port? }` — `port` = 조건 노드의 출력 갈래 (`"true"`/`"false"`,
    인코딩은 `"out"`, 액션의 체인 출력이 있음)

## 2. 실행 흐름 (Scan → Encode → Post chain)

```
auto-scan (5분 주기, CT_AUTO_SCAN)
  └─ 파일마다 (6병렬 프로브, 무변경 재사용)
      └─ evaluate_graph()            ← 결정 단계 (backend/src/main.rs)
          ① 폴더의 logic_id → NamedLogic.graph     (로직 탭에서 부여)
          ② 폴더의 레거시 인라인 graph
          ③ 전역 라이브러리 그래프
          ④ 이상 없음 → 폴더 일괄 기본값 (folder.target / auto-encode 설정)
          └─ eval_graph(): start부터 DFS, 첫 도착한 액션이 승리
              ├─ 조건 노드: cond_matches() (sync, 싼 조건)
              │              + cmd 플러그인 (async, 셸 실행)
              └─ srcHealth 게이트: LAZY — true/false 둘 다 평가해 결과가
                  동일하면 디코드를 아예 안 함 (결과가 갈라지는 파일만 디코드 비용)
      └─ GraphDecision::Encode { target, post: Vec<PostOp>, hw_only } | Skip
          └─ 노드 스케줄링 (가용 HW + hwOnly 게이트 + least-busy 큐잉, 큐 상한 10)
              └─ 인코딩 → Post chain
                  post_chain(): 인코딩 출력 포트의 후속 노드를 체인 순서로
                  ├─ 조건: 같은 health map으로 재평가, 일치하는 갈래만 추적
                  ├─ 액션: PostOp 생성 (verify/replace/saveHere/requeue/postCmd)
                  └─ 재귀 깊이 8, 사이클 방지 (visited)
```

- **hwOnly 인코딩**: `hw_only=true`면 해당 코덱의 HW 인코더를 가진 노드만 수락,
  없으면 에러(노드 재시도 대기). AV1 = a node with av1_nvenc (e.g. RTX PRO 4000) 전용.
- **PostOp 실행 시점**: entry 락을 **해제한 뒤** (Cmd 실행 중엔 entry 점유 없음 —
  `PostOp::Cmd`는 실행 직전에 필요한 값만 락 밖으로 클론).

## 3. 조건 (Condition) 카탈로그

| kind | 의미 | 판정 |
|---|---|---|
| `always` | 항상 통과 | true |
| `srcCodec` | 원본 코덱 ∈ 선택 집합 | `codec_family` 정규화 (h264/hevc/av1/vp9 계열) |
| `sizeGt` / `sizeLt` | 파일 크기 MB | 프로브 |
| `heightGte` | 세로 해상도 ≥ | 프로브 |
| `bitrateGt` / `bitrateLt` | 원본 비트레이트 Mbps | 프로브 |
| `nameContains` | 파일명 부분 일치 | 프로브 |
| `srcHealth` | 프레임 디코드 헬스체크 (전체/앞N%/무작위) | `decode_health_check`, `CT_HEALTH_WORKERS` 병렬 상한 |
| `nodeHasHw` | 온라인 노드 중 코덱+가속기 클래스 HW 인코더 보유 | 노드 registry |
| **`field`** 🧩 | **임의 프로브 필드 × 연산자** | 14 필드 (size_mb/size/duration_s/width/height/fps/bitrate×3/name/codec/container/pixel_fmt) × 9 연산자 (gt/gte/lt/lte/eq/neq/contains/startsWith/endsWith) — 코덱은 계열 정규화 후 비교 |
| **`cmd`** 🧩 | **셸 명령 exit 0 = ✓** | 클린 env + `PATH=/usr/local/bin:/usr/bin:/bin:/share/.codectrans/scripts`, env 주입: `CT_FILE CT_NAME CT_SIZE CT_SIZE_MB CT_CODEC CT_CONTAINER CT_PIXEL_FMT CT_DURATION_S CT_WIDTH CT_HEIGHT CT_FPS CT_BITRATE CT_VIDEO_BITRATE CT_AUDIO_BITRATE`, timeout 1~300초 (기본 30, 초과=✗) |

## 4. 액션 (Action) 카탈로그

| kind | 시점 | 동작 |
|---|---|---|
| `encode` | 주 결정 | 코덱/CRF 또는 원본 % (10~100, maxrate 하드캡) / hwOnly / 인코더 우선순위 choices |
| `skip` | 주 결정 | 인코딩하지 않음 |
| `failToFolder` | 주 결정(✗ 갈래) | 원본을 실패 보관 폴더로 이동 (세팅 탭 지정) |
| `verify` | 인코딩 후 | 출력 ffprobe (전체 / N% / N에 1) — 실패 시 `verify_on_failure`: error(기본) \| requeue |
| `replace` | 인코딩 후 | 출력이 원본 경로 대치 (`.orig` 백업 — **완전 대치 모드에서는 백업 없이 교체**되도록 세팅됨) |
| `saveHere` | 인코딩 후 | 출력을 원본 옆에 (기본 동작 명시) |
| `requeue` | 인코딩 후 | 파일을 큐로 복귀 (재스캔 시 재처리) |
| **`postCmd`** 🧩 | **인코딩 후 (체인 순서)** | 셸 명령 실행 — env: `CT_OUTPUT CT_OUTPUT_NAME CT_OUTPUT_SIZE_MB CT_INPUT CT_INPUT_NAME CT_STATUS` + 원본 메타. **비치명적**: 실패=경고 로그, 체인 계속. 파일 운명은 스크립트가 결정하지 못하게 잠금 |

## 5. 확장 훅 (새 기능 추가 위치)

1. **Rust** (`backend/src/main.rs`)
   - `Condition`/`Action` enum: serde `#[serde(tag="kind", rename_all="camelCase")]` —
     필드명은 **camelCase** (`bitratePct`, `cmdTimeout`)
   - `cond_matches()` — 새 조건 판정 (async: cmd 계열)
   - `eval_graph()` — 조건 분기, `post_chain()` — 체인 후속
   - ⚠ 교훈: 로컬 `async fn` 항목은 동적 환경 캡처 불가 (E0434) → 자유 함수로;
     재귀 async fn은 `BoxFuture` 인접 (E0733)
2. **프론트** (`frontend/src/App.jsx`)
   - `nodeTitle()` — 노드 카드 헤더
   - `addNode()` preset + `<Pal>` 팔레트 + 노드 폼 (kind select option + per-kind 폼)
   - 빌드: `npm run build` → **dist 재빌드 누락이 실사용 버그의 1차 원인**
3. **배포**: `docker build -f docker/server.Dockerfile` → Docker Hub push →
   QNAP 컨테이너 재생성 (docker CLI, compose 없음) — 서버 리스타트 0건 목표는
   핫스왑으로 달성 (진행 중 잡은 다음 스캔에 재큐잉)

## 6. 보안/안전 규칙 (플러그인 실행)

- cmd/postCmd는 **서버 컨테이너의 non-root 사용자**로 실행 — root 권한 없음
- 스크립트 경로는 `PATH`의 `/share/.codectrans/scripts` (볼륨 마운트) 권장 —
  절대경로 셸 명령도 가능하지만 컨테이너 내부 경로만 존재
- env는 **완전 클린** (호스트 환경 변수 유출 없음) + 명시적 CT_* 주입
- timeout 강제 (kill), 조건 실패/타임아웃 = ✗ 갈래 (파일은 안전 — 처리 안 됨)
- postCmd 실패는 **경고 로그만** — 인코딩된 파일 운명에 영향을 주지 못함
