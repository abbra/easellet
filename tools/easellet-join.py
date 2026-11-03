#!/usr/bin/env python3
"""Join a Wi-Fi network from the easellet console, then persist it to the card.

The password is read with getpass (no echo) and never printed. It travels over
the USB serial link, and - only if you say yes - is written to
/sdcard/EASELLET/CONFIG.TXT so a reboot can re-join without retyping it.

The port is opened once and held, and that is not a detail: the USB-Serial/JTAG
interface resets the chip when the host opens or closes the port, and the driver
keeps its Wi-Fi configuration in RAM. A join made in one session is gone by the
next, so join -> status -> SNTP -> `wifi auto on` -> `config save` all run here
before the port closes.

Run it from the project root:

    .venv-tools/bin/python tools/easellet-join.py

Answering yes to the save prompt makes the next boot join by itself, which is
reported by `info` as `wifi_auto=connected` with an address from `wifi status`.
"""
import getpass
import sys
import time

import serial

PORT = "/dev/ttyACM0"
BAUD = 115200
DEFAULT_SSID = "boids"


def command(port: serial.Serial, line: str, timeout: float = 120.0) -> str:
    port.write(line.encode() + b"\r")
    port.flush()
    reply = b""
    deadline = time.time() + timeout
    while time.time() < deadline:
        chunk = port.read(4096)
        if chunk:
            reply += chunk
            if b"\r\n" in reply:
                break
        else:
            time.sleep(0.02)
    return reply.decode(errors="replace").strip()


def ask_yes(text: str) -> bool:
    return input(f"{text} [y/N]: ").strip().lower() in {"y", "yes"}


def main() -> int:
    ssid = DEFAULT_SSID
    password = ""
    scripted = len(sys.argv) >= 3
    # Arguments exist for scripted tests only; the normal path prompts.
    if scripted:
        ssid, password = sys.argv[1], sys.argv[2]
    else:
        entered = input(f"SSID [{DEFAULT_SSID}]: ").strip()
        if entered:
            ssid = entered
        password = getpass.getpass("Wi-Fi password (no echo, not stored): ")
    if not ssid or " " in ssid:
        print("ERR ssid must be non-empty and contain no spaces")
        return 2

    with serial.Serial(PORT, BAUD, timeout=0.2) as port:
        port.dtr = False
        port.rts = False
        time.sleep(0.2)
        port.reset_input_buffer()

        replies = [(f"wifi join {ssid} <hidden>", command(port, f"wifi join {ssid} {password}"))]
        if not replies[0][1].startswith("OK"):
            for label, reply in replies:
                print(f"$ {label}\n{reply or '<no reply>'}")
            return 1
        replies.append(("wifi status", command(port, "wifi status")))

        # Everything below must happen before the port closes, or the reset
        # triggered on close would drop the RAM-only credentials.
        if scripted and "--time" in sys.argv:
            do_time = True
        else:
            do_time = ask_yes("Fetch time over SNTP and write it to the PCF85063 RTC?")
        if do_time:
            replies.append(("wifi time", command(port, "wifi time", timeout=60.0)))
            replies.append(("rtc", command(port, "rtc")))

        # Persistence must happen while the credentials are still in RAM. Ask,
        # because this puts the passphrase in the clear on the card.
        if scripted and "--save" in sys.argv:
            do_save = True
        else:
            do_save = ask_yes(
                "Write these credentials and wifi.auto=1 to EASELLET/CONFIG.TXT? "
                "(passphrase stored in the clear on the SD card)"
            )
        if do_save:
            replies.append(("wifi auto on", command(port, "wifi auto on", timeout=30.0)))
            replies.append(("config save", command(port, "config save", timeout=60.0)))
            replies.append(("config", command(port, "config")))
            replies.append(("sd ls EASELLET", command(port, "sd ls EASELLET")))
        replies.append(("wifi off", command(port, "wifi off", timeout=30.0)))

    for label, reply in replies:
        if password and password in reply:
            reply = reply.replace(password, "<redacted by easellet-join>")
        print(f"$ {label}\n{reply or '<no reply>'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
