# lavalock

A screen locker that shows colorful blobs drifting across the screen. It
mimics the "Goop" hack (`hacks/goop.c`) from Jamie Zawinski's
[XScreenSaver](https://www.jwz.org/xscreensaver/), in its opaque mode as
seen on OpenBSD/xenocara.

Runs on Wayland (via `ext-session-lock`) and X11, and unlocks with the user's
password (PAM, or BSD Authentication on OpenBSD).

## Build

    cargo build --release

The default renderer uses OpenGL. CPU renderers are available with
`--no-default-features --features renderer-softbuffer` or
`renderer-pixels`.

## Usage

    lavalock [-n] [-d seconds]

- `-n`, `--nofork` stays in the foreground until unlocked, instead of going
  to the background once the screen is locked.
- `-d`, `--display-off` turns the displays off after that many seconds
  without input.

To lock after 3 minutes of inactivity, with the displays turning off 5
minutes later, and before the system suspends, use
[swayidle](https://github.com/swaywm/swayidle):

    swayidle -w timeout 180 'lavalock -d 300' before-sleep lavalock

With `-w`, the system only suspends once the screen is locked. If
the session is already locked, the second lavalock exits on its own.
