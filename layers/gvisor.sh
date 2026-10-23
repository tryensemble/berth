# A gVisor layer for strict berths: runsc, from its GitHub release, verified.
# Build on top of tools, and create strict berths with it:
#   berth layer build gvisor layers/gvisor.sh --from tools
#   berth create --layer tools --layer gvisor --strict
set -e
apt-get update -qq && apt-get install -y -qq --no-install-recommends bzip2 ca-certificates curl >/dev/null
rel=$(curl -fsSL https://api.github.com/repos/google/gvisor/releases/latest \
  | sed -n 's/.*"tag_name": *"\(release-[0-9.]*\)".*/\1/p' | head -1)
arch=$(uname -m)
cd /tmp
curl -fsSLO "https://github.com/google/gvisor/releases/download/$rel/gvisor-$arch.tar.bz2"
curl -fsSLO "https://github.com/google/gvisor/releases/download/$rel/SHA512SUMS"
grep "gvisor-$arch.tar.bz2" SHA512SUMS | sha512sum -c -
mkdir -p /opt/gvisor && tar xjf "gvisor-$arch.tar.bz2" -C /opt/gvisor
ln -sf /opt/gvisor/runsc /usr/local/bin/runsc
rm -rf /tmp/* /var/lib/apt/lists/*
runsc --version | head -1
