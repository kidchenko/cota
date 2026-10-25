#!/bin/sh
# Installs Cota from the .deb in /src/dist, then starts the desktop as a normal
# user, with Cota in its tray and logging at debug level. The VNC server has no
# password, so publish the port on localhost only, as the Dockerfile shows.
set -e
deb="/src/dist/cota_$(dpkg --print-architecture).deb"
[ -f "$deb" ] || { echo "no $deb: build it first (see the Dockerfile)" >&2; exit 1; }
apt-get install -y -qq "$deb" >/dev/null
if [ -f /creds/.credentials.json ]; then
  install -o me -m 600 -D /creds/.credentials.json /home/me/.claude/.credentials.json
fi
su me -c '
  export HOME=/home/me DISPLAY=:1 XDG_CURRENT_DESKTOP=XFCE
  vncserver :1 -geometry 1280x800 -SecurityTypes None -localhost no --I-KNOW-THIS-IS-INSECURE \
    -xstartup /usr/bin/startxfce4 >/tmp/vnc.log 2>&1
  sleep 6
  nohup sh -c "COTA_LOG=debug cota" >/tmp/cota.out 2>&1 &
'
echo "desktop ready: http://localhost:6080/vnc.html"
exec websockify --web /usr/share/novnc 6080 localhost:5901
