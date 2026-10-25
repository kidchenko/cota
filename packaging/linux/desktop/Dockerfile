# A Linux desktop to try Cota in, from a Mac or anywhere else: XFCE with a tray
# that hosts AppIndicators, a notification daemon, and noVNC to look at it in a
# browser. Cota is installed from the .deb, so the package is tested too.
#
#   docker build -t cota-linux packaging/linux
#   docker build -t cota-desktop packaging/linux/desktop
#   docker run --rm -v "$PWD":/src -v cota-target:/target cota-linux ./build.sh --package
#   docker run --rm -p 127.0.0.1:6080:6080 -v "$PWD":/src:ro cota-desktop
#
# then open http://localhost:6080/vnc.html and click Connect. Cota is the ring
# at the top right; its log is in /home/me/.config/cota/cota.log.
#
# Without a token the panel says "not signed in". To see real numbers, put a
# Claude Code credentials file in a folder of its own and add
# `-v <that folder>:/creds:ro`. On a Mac the token is in the Keychain:
#
#   mkdir -m 700 /tmp/cota-creds && security find-generic-password \
#     -s "Claude Code-credentials" -w > /tmp/cota-creds/.credentials.json
#
# Delete that folder when you are done: it is your live token, in plain text.
FROM cota-linux
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        xfce4-session xfce4-panel xfwm4 xfdesktop4 xfce4-notifyd xfce4-settings \
        tigervnc-standalone-server novnc websockify xdotool adwaita-icon-theme \
        at-spi2-core \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m me
COPY start.sh /start.sh
EXPOSE 6080
CMD ["sh", "/start.sh"]
