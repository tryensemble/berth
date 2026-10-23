# A Docker layer, on top of tools: dockerd, the CLI and compose, for berths
# that run containers. Each berth gets its own Docker, started the first time
# something uses /var/run/docker.sock.
#   berth layer build docker layers/docker.sh --from tools
set -e
apt-get update -qq
apt-get install -y -qq --no-install-recommends docker.io docker-compose-v2 iptables >/dev/null
# dockerd is started by the berth's init, not by systemd.
rm -f /etc/systemd/system/multi-user.target.wants/docker.service
usermod -aG docker agent
rm -rf /var/lib/apt/lists/* /tmp/*
docker --version
docker compose version
