#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
output_dir="$project_root/docs/screenshots"
binary="${1:-}"

if [[ "$#" -gt 1 ]]; then
	echo "usage: $0 [existing-desktop-binary]" >&2
	exit 1
fi

for command_name in import mogrify sqlite3 Xvfb xdotool; do
	if ! command -v "$command_name" >/dev/null 2>&1; then
		echo "missing required command: $command_name" >&2
		exit 1
	fi
done

if [[ -z "$binary" ]]; then
	if ! command -v cargo >/dev/null 2>&1; then
		echo "missing required command: cargo (or pass an existing desktop binary)" >&2
		exit 1
	fi
	# Use the low-memory profile and one compiler job; enforce any hard memory
	# limit with the caller's container or cgroup.
	CARGO_INCREMENTAL=0 cargo build --manifest-path "$project_root/Cargo.toml" \
		--locked --profile ci --jobs 1 --config profile.ci.package.sharer.opt-level=0 \
		--features desktop --bin ShareR
	binary="${CARGO_TARGET_DIR:-$project_root/target}/ci/ShareR"
fi
if [[ ! -x "$binary" ]]; then
	echo "desktop binary is not executable: $binary" >&2
	exit 1
fi

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/sharer-screenshots.XXXXXX")"
app_pid=""
xvfb_pid=""

cleanup() {
	if [[ -n "$app_pid" ]]; then
		kill "$app_pid" 2>/dev/null || true
		wait "$app_pid" 2>/dev/null || true
	fi
	if [[ -n "$xvfb_pid" ]]; then
		kill "$xvfb_pid" 2>/dev/null || true
		wait "$xvfb_pid" 2>/dev/null || true
	fi
	rm -rf -- "$work_dir"
}
trap cleanup EXIT INT TERM

display_number=90
while [[ -e "/tmp/.X11-unix/X$display_number" ]]; do
	display_number=$((display_number + 1))
done
display_value=":$display_number"

Xvfb "$display_value" -screen 0 2048x1400x24 -nolisten tcp >"$work_dir/xvfb.log" 2>&1 &
xvfb_pid=$!

for _ in {1..50}; do
	if DISPLAY="$display_value" xdotool getmouselocation >/dev/null 2>&1; then
		break
	fi
	sleep 0.1
done
if ! DISPLAY="$display_value" xdotool getmouselocation >/dev/null 2>&1; then
	echo "Xvfb did not become ready" >&2
	exit 1
fi

config_root="$work_dir/config"

XDG_CONFIG_HOME="$config_root" SHARER_CONFIG_DIR="$config_root/sharer" "$binary" --init https://uploads.example/upload >/dev/null
database="$config_root/sharer/sharer.sqlite3"

sqlite3 "$database" <<'SQL'
UPDATE settings
SET value = CAST(json_set(
    CAST(value AS TEXT),
    '$.capture_directory', '/home/example/Pictures/ShareR',
    '$.auto_upload_captures', json('true'),
    '$.completion_sound', json('true'),
    '$.copy_link_after_upload', json('true'),
    '$.onboarding_complete', json('true')
) AS BLOB)
WHERE key = 'application';
DELETE FROM upload_history;
INSERT INTO upload_history (created_at, original_name, size_bytes, link, delete_url, expires_at)
VALUES (1789574400, 'design-review.png', 184320, 'https://files.example/a1B2c3.png', 'https://files.example/delete/a1B2c3/token', '2026-09-16 20:14 UTC');
INSERT INTO upload_history (created_at, original_name, size_bytes, link, delete_url, expires_at)
VALUES (1789572600, 'release-notes.pdf', 2411725, 'https://files.example/d4E5f6.pdf', 'https://files.example/delete/d4E5f6/token', '2026-09-16 19:42 UTC');
INSERT INTO upload_history (created_at, original_name, size_bytes, link, delete_url, expires_at)
VALUES (1789569000, 'capture-hdr.jpg', 7340032, 'https://files.example/g7H8i9.jpg', 'https://files.example/delete/g7H8i9/token', '2026-09-16 18:03 UTC');
SQL

DISPLAY="$display_value" \
	XDG_CONFIG_HOME="$config_root" \
	SHARER_CONFIG_DIR="$config_root/sharer" \
	SLINT_BACKEND=winit-software \
	SLINT_SCALE_FACTOR=2 \
	HTTPS_PROXY=http://192.0.2.1:9 \
	"$binary" >"$work_dir/sharer.log" 2>&1 &
app_pid=$!

window_id=""
for _ in {1..100}; do
	window_ids="$(DISPLAY="$display_value" xdotool search --onlyvisible --name '^ShareR$' 2>/dev/null || true)"
	if [[ -n "$window_ids" ]]; then
		window_id="${window_ids%%$'\n'*}"
		break
	fi
	if ! kill -0 "$app_pid" 2>/dev/null; then
		echo "ShareR exited before opening a window" >&2
		sed -n '1,160p' "$work_dir/sharer.log" >&2
		exit 1
	fi
	sleep 0.1
done
if [[ -z "$window_id" ]]; then
	echo "ShareR window did not appear" >&2
	exit 1
fi

mkdir -p "$output_dir"

click_at() {
	DISPLAY="$display_value" xdotool mousemove --sync --window "$window_id" "$1" "$2" click 1
	sleep 0.2
}

capture_view() {
	local filename="$1"
	DISPLAY="$display_value" import -window "$window_id" "$output_dir/$filename"
	mogrify -strip "$output_dir/$filename"
}

sleep 0.4
capture_view capture.png

click_at 170 284
capture_view history.png

click_at 170 376
click_at 456 178
capture_view settings-general.png

click_at 600 178
capture_view settings-capture.png

click_at 738 178
capture_view settings-upload.png

click_at 870 178
capture_view settings-privacy.png

# First-run setup needs an empty configuration directory.
kill "$app_pid" 2>/dev/null || true
wait "$app_pid" 2>/dev/null || true
app_pid=""
window_id=""

DISPLAY="$display_value" \
	XDG_CONFIG_HOME="$work_dir/fresh-config" \
	SHARER_CONFIG_DIR="$work_dir/fresh-config/sharer" \
	SLINT_BACKEND=winit-software \
	SLINT_SCALE_FACTOR=2 \
	HTTPS_PROXY=http://192.0.2.1:9 \
	"$binary" >"$work_dir/sharer-fresh.log" 2>&1 &
app_pid=$!

for _ in {1..100}; do
	window_ids="$(DISPLAY="$display_value" xdotool search --onlyvisible --name '^ShareR$' 2>/dev/null || true)"
	if [[ -n "$window_ids" ]]; then
		window_id="${window_ids%%$'\n'*}"
		break
	fi
	if ! kill -0 "$app_pid" 2>/dev/null; then
		echo "ShareR exited before opening the first-run window" >&2
		sed -n '1,160p' "$work_dir/sharer-fresh.log" >&2
		exit 1
	fi
	sleep 0.1
done
if [[ -z "$window_id" ]]; then
	echo "ShareR first-run window did not appear" >&2
	exit 1
fi

sleep 0.4
capture_view onboarding.png

for screenshot in \
	capture.png \
	history.png \
	settings-upload.png \
	settings-capture.png \
	settings-privacy.png \
	settings-general.png \
	onboarding.png; do
	if [[ ! -s "$output_dir/$screenshot" ]]; then
		echo "failed to create $screenshot" >&2
		exit 1
	fi
done

echo "Generated screenshots in $output_dir"
