# A desktop layer: a virtual screen with Google Chrome on it, which an agent
# can drive and screenshot and you can watch in a browser. Goes on top of
# the tools layer.
#   berth layer build desktop layers/desktop.sh --from tools
#   berth desktop ID        # starts the screen, prints a URL to watch it
#   berth screenshot ID     # saves what's on it
set -e
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    xvfb x11vnc openbox novnc websockify scrot xdotool \
    fonts-noto-core fonts-noto-cjk fonts-noto-color-emoji ca-certificates curl gnupg >/dev/null
# Chrome from Google's own repository, its packages checked against its key.
curl -fsSL https://dl.google.com/linux/linux_signing_key.pub | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg
echo "deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] https://dl.google.com/linux/chrome/deb/ stable main" \
    > /etc/apt/sources.list.d/google-chrome.list
apt-get update -qq
apt-get install -y -qq --no-install-recommends google-chrome-stable >/dev/null

# The screen, run as a berth service: a virtual display (:0), a window
# manager, and a view of it in a browser at port 6080 (noVNC), all on the
# berth's own loopback. If any part stops, all of it stops, and the service
# starts it again. One screen at a time: a second one waits on the lock until
# the first stops, then takes over.
touch /usr/local/share/berth-desktop.lock
cat > /usr/local/bin/berth-desktop <<'EOF'
#!/bin/bash
set -e
exec 9</usr/local/share/berth-desktop.lock
flock 9
export DISPLAY=:0
size=${BERTH_DESKTOP_SIZE:-1920x1080}
rm -f /tmp/.X0-lock /tmp/.X11-unix/X0
Xvfb :0 -screen 0 "${size}x24" -nolisten tcp &
for _ in $(seq 100); do [ -S /tmp/.X11-unix/X0 ] && break; sleep 0.05; done
openbox &
x11vnc -display :0 -localhost -rfbport 5900 -forever -shared -nopw -quiet &
websockify --web /usr/share/novnc 127.0.0.1:6080 127.0.0.1:5900 &
# When any part stops, stop the rest: all of them, as each holds the lock
# and websockify ignores SIGTERM.
wait -n || true
kill -KILL 0
EOF
chmod 755 /usr/local/bin/berth-desktop

# Chrome on that screen, with DevTools on the berth's port 9222 for tools
# that drive it. With no screen yet, it starts one.
cat > /usr/local/bin/berth-browser <<'EOF'
#!/bin/sh
export DISPLAY="${DISPLAY:-:0}"
if flock -n /usr/local/share/berth-desktop.lock true; then
    setsid -f berth-desktop >/tmp/berth-desktop.log 2>&1 </dev/null
fi
for _ in $(seq 100); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
exec google-chrome --user-data-dir="$HOME/.config/berth-chrome" \
    --remote-debugging-address=127.0.0.1 --remote-debugging-port=9222 \
    --no-first-run --no-default-browser-check --start-maximized "$@"
EOF
chmod 755 /usr/local/bin/berth-browser

rm -rf /var/lib/apt/lists/* /tmp/*
google-chrome --version
