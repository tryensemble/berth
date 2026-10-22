# A tools layer for agent threads: git, Node and Claude Code, and the agent
# user (uid 1000) commands run as by default, with sudo to the berth's root.
#   berth layer build tools layers/tools.sh
set -e
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl git xz-utils procps iproute2 less sudo >/dev/null
curl -fsSL https://nodejs.org/dist/v22.20.0/node-v22.20.0-linux-x64.tar.xz | tar xJ -C /usr/local --strip-components=1
npm install -g --silent @anthropic-ai/claude-code
userdel -r ubuntu 2>/dev/null || true
useradd -m -u 1000 -s /bin/bash agent
# The agent becomes the berth's root for installs; that root is no one outside.
echo 'agent ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/agent
chmod 440 /etc/sudoers.d/agent
rm -rf /var/lib/apt/lists/* /root/.npm /tmp/*
node --version
claude --version
