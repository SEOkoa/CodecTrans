#!/bin/bash
# CodecTrans Mac node supervisor — keeps the node alive inside a persistent
# SSH session (macOS quirk: processes reparented out of the SSH session
# lose LAN routing on this Mac — "No route to host").
# The node runs in the FOREGROUND of the SSH connection; when it exits,
# we reconnect. sshd holds the session (and its network context) open.
#
# Configure (never commit real values):
#   export MAC_USER="mac-user" MAC_HOST="192.168.x.x" MAC_PASS="..."
#   export CT_SERVER="http://192.168.x.x:8400"
#   export CT_WORK_DIR="/Users/<user>/CodecTrans/CodecTrans-node"
#   export CT_NFS="/Users/<user>/nfs"        # NFS mount on the Mac
: "${MAC_USER:?set MAC_USER}"
: "${MAC_HOST:?set MAC_HOST (LAN IP of the Mac)}"
: "${MAC_PASS:?set MAC_PASS (Mac user password)}"
: "${CT_SERVER:?set CT_SERVER (e.g. http://192.168.x.x:8400)}"
: "${CT_WORK_DIR:?set CT_WORK_DIR (absolute node dir on the Mac)}"
: "${CT_NFS:?set CT_NFS (absolute NFS mount path on the Mac)}"
SSH="sshpass -p $MAC_PASS ssh -o StrictHostKeyChecking=no -o ServerAliveInterval=15 -o ServerAliveCountMax=4 ${MAC_USER}@${MAC_HOST}"
REMOTE_CMD="export PATH=/opt/homebrew/bin:\$PATH CT_SERVER=${CT_SERVER} CT_NODE_NAME=\"Mac Studio\" CT_WORK_DIR=${CT_WORK_DIR} CT_PATH_MAP=/media:${CT_NFS}; cd ~/CodecTrans/CodecTrans-node && exec ./target/release/CodecTransNode"

echo "[supervisor] starting (Mac node via persistent ssh)"
while true; do
    echo "[supervisor] $(date -u +%FT%TZ) connecting..."
    $SSH "$REMOTE_CMD" >> /tmp/ct_mac_node.log 2>&1
    RC=$?
    echo "[supervisor] $(date -u +%FT%TZ) node exited rc=$RC — reconnecting in 10s"
    sleep 10
done
