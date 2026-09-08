#!/usr/bin/env bash
# Install the Garden brain onto a Fedora host.
#
# Idempotent: safe to re-run after editing a unit or rebuilding the image. It will
# not overwrite /etc/garden/web.env or /etc/garden/ntfy-server.yml once they exist,
# because those hold your secrets and your URLs.
#
# Run from the repository root:
#     sudo ./deploy/install.sh
#
# DEPLOYMENT.md explains every step this performs, and why. Read that first if
# anything here fails — this script is the short path, not the explanation.

set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "run with sudo" >&2
  exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(dirname "$HERE")"
# The uid the containers run as. Kept as two values as well as one string because
# `install` wants them separately (`-o` is a user, not a user:group) while `chown`
# wants them joined — passing the combined form to `install` fails with the
# unhelpful "invalid user '1000:1000'".
RUN_UID=1000
RUN_GID=1000
UID_GID="$RUN_UID:$RUN_GID"

say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

say "Checking prerequisites"
for tool in podman sqlite3 systemctl; do
  command -v "$tool" >/dev/null || { echo "missing: $tool (dnf install $tool)" >&2; exit 1; }
done
# Quadlet arrived in podman 4.4. On anything older the .container files are
# silently ignored and you get no error at all, which is a miserable way to spend
# an evening.
version="$(podman version --format '{{.Client.Version}}')"
case "$version" in
  [0-3].*|4.[0-3].*) echo "podman $version is too old for Quadlet; need 4.4+" >&2; exit 1 ;;
esac
echo "podman $version"

say "Creating directories"
install -d -o "$RUN_UID" -g "$RUN_GID" -m 0750 /var/lib/garden /var/lib/garden/db \
  /var/lib/garden/frames /var/lib/garden/backups
install -d -o "$RUN_UID" -g "$RUN_GID" -m 0750 /var/lib/garden-ntfy /var/cache/garden-ntfy
# Caddy's certificates and its ACME account key. Losing this directory means
# re-issuing on every restart, which Let's Encrypt rate-limits hard.
install -d -m 0750 /var/lib/garden-caddy /var/log/garden-caddy
install -d -m 0750 /etc/garden

say "Installing configuration templates"
for pair in "web.env.example:/etc/garden/web.env" \
            "ntfy-server.yml:/etc/garden/ntfy-server.yml" \
            "Caddyfile:/etc/garden/Caddyfile"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  if [ -e "$dst" ]; then
    echo "keeping existing $dst"
  else
    install -m 0600 "$HERE/$src" "$dst"
    echo "created $dst — EDIT THIS BEFORE STARTING"
  fi
done
# ntfy reads its config as uid 1000 and will not start if it cannot.
chown "$UID_GID" /etc/garden/ntfy-server.yml

say "Installing Quadlet units"
install -d -m 0755 /etc/containers/systemd
for unit in garden.network garden-web.container garden-ntfy.container \
            garden-caddy.container; do
  install -m 0644 "$HERE/quadlet/$unit" "/etc/containers/systemd/$unit"
  echo "  $unit"
done
echo "  (garden-ollama.container not installed — see DEPLOYMENT.md, optional)"

say "Installing the backup job"
install -m 0755 "$HERE/garden-backup" /usr/local/bin/garden-backup
install -m 0644 "$HERE/systemd/garden-backup.service" /etc/systemd/system/
install -m 0644 "$HERE/systemd/garden-backup.timer" /etc/systemd/system/

say "Building the image (this takes several minutes the first time)"
podman build -t localhost/garden-web:latest -f "$HERE/Containerfile" "$REPO"

say "Reloading systemd"
# Quadlet is a systemd *generator*: daemon-reload is what turns the .container
# files above into real .service units. Without it, nothing exists to start.
systemctl daemon-reload

say "Opening the firewall"
# Internal zone only, both of them. This is a LAN deployment until you decide
# otherwise: the phone gets notifications on wifi and nothing is reachable from the
# internet. `garden-caddy` and its public ports are a later, deliberate step —
# DEPLOYMENT.md Part 7, and DESIGN.md §10 for why only ntfy ever goes out there.
if systemctl is-active --quiet firewalld; then
  firewall-cmd --permanent --zone=internal --add-port=8080/tcp >/dev/null  # the brain
  firewall-cmd --permanent --zone=internal --add-port=8090/tcp >/dev/null  # ntfy
  firewall-cmd --reload >/dev/null
  echo "8080 and 8090 open on the internal zone; nothing public"
else
  echo "firewalld is not running — skipped"
fi

cat <<'DONE'

==> Installed. Three things left, all of which need your input:
     (this gets you a LAN deployment — Caddy and DNS come later, if at all)

  1. Edit /etc/garden/ntfy-server.yml — set base-url to this VM's LAN address
     and port, e.g. http://192.168.1.20:8090. That is how the PHONE reaches it,
     so `localhost` is always wrong. Then:

       sudo systemctl start garden-ntfy
       sudo podman exec -it systemd-garden-ntfy ntfy user add --role=admin garden
       sudo podman exec -it systemd-garden-ntfy ntfy token add garden

  2. Edit /etc/garden/web.env — set GARDEN_BASE_URL to the brain's LAN address
     (it stays on the LAN by design; see DESIGN.md §10), paste the ntfy token
     into GARDEN_NTFY_TOKEN, and generate an agent token:

       openssl rand -hex 32

     Then:

       sudo systemctl start garden-web
       sudo systemctl enable --now garden-backup.timer

  3. Subscribe the phone. ntfy app -> Settings -> Default server -> the same LAN
     URL, sign in as `phone`, subscribe to an unguessable topic, and paste that
     topic into Account -> Notification settings in the web UI.

  That is a working LAN deployment: notifications arrive whenever the phone is on
  your wifi, and nothing is reachable from the internet.

  For notifications away from home, `garden-caddy` fronts ntfy with a real
  certificate — DEPLOYMENT.md Part 7. It needs a DNS name and a forwarded port,
  so it is a deliberate later step rather than part of this install.

  Watch it come up with:  journalctl -u garden-web -f
  The first account to register at the web UI becomes the administrator —
  unless you migrated an existing database, which brings its accounts with it.

DONE
