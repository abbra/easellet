#!/usr/bin/env python3
"""Generate request vectors with py-opendisplay itself.

Run with the project venv that has the client installed:

    .tools/venv-opendisplay/bin/python \
        opendisplay-proto/tests/fixtures/make_client_vectors.py

The client parses board-config.bin (the config the device will report), then
builds every request from it exactly as it would against the frame: commands
from protocol/commands.py, and the image through prepare_image(), which fits,
dithers, rotates by DisplayConfig.rotation, packs and compresses. Outputs:

    client-vectors.txt   one "label hex" line per request frame
    client-frame.zlib    the packed 192,000-byte image prepare_image produced
                         (plain zlib, only to keep the file small)

tests/client.rs replays the vectors through the Rust server.
"""

import pathlib
import zlib

from PIL import Image

from opendisplay import prepare_image
from opendisplay.protocol import commands as c
from opendisplay.protocol.config_parser import parse_config_response
from opendisplay.protocol.config_serializer import serialize_config

HERE = pathlib.Path(__file__).parent
LAN_CHUNK = 4094 - 2  # device.py _direct_write_chunk_size on a LAN transport
ETAG = 0x1234ABCD


def test_image() -> Image.Image:
    """White, a red square at the viewer's top-left corner, and a colour
    gradient band across the middle so dithering gives the compressor work."""
    image = Image.new("RGB", (800, 480), (255, 255, 255))
    image.paste((255, 0, 0), (0, 0, 100, 100))
    for x in range(800):
        for y in range(150, 350):
            image.putpixel((x, y), (x * 255 // 799, (y - 150) * 255 // 199, 128))
    return image


def main() -> None:
    config = parse_config_response((HERE / "board-config.bin").read_bytes())
    display = config.displays[0]
    assert (display.pixel_width, display.pixel_height) == (800, 480)
    assert (display.panel_ic_type, display.color_scheme, display.rotation) == (35, 4, 2)

    raw, compressed, _ = prepare_image(test_image(), config=config)
    assert len(raw) == 192_000
    # Rotation 2 means the client turns the picture 180 degrees before
    # packing, so the viewer's top-left red lands at the end of the buffer,
    # which is where the panel scans the viewer's top-left from.
    assert raw[-1] == 0x33 and raw[0] == 0x11, (raw[:4].hex(), raw[-4:].hex())

    lines: list[str] = []

    def add(label: str, frame: bytes) -> None:
        lines.append(f"{label} {frame.hex()}")

    add("read_config", c.build_read_config_command())
    add("fw_version", c.build_read_fw_version_command())
    add("read_msd", c.build_read_msd_command())
    add("reboot", c.build_reboot_command())
    add("deep_sleep_300", c.build_deep_sleep_command(300))
    add("deep_sleep_default", c.build_deep_sleep_command(None))
    add("power_off", c.build_power_off_command())
    add("auth_step1", c.build_authenticate_step1())

    blob = serialize_config(config)
    add("config_blob", blob)
    first, chunks = c.build_write_config_command(blob)
    add("config_write", first)
    for chunk in chunks:
        add("config_chunk", chunk)

    # LAN upload: compressed START (<= 200 bytes), 4092-byte DATA, END + etag.
    start, rest = c.build_direct_write_start_compressed(len(raw), compressed)
    add("lan_start", start)
    for i in range(0, len(rest), LAN_CHUNK):
        add("lan_data", c.build_direct_write_data_command(rest[i : i + LAN_CHUNK], max_data_len=LAN_CHUNK))
    add("lan_end", c.build_direct_write_end_with_etag(0, ETAG))

    # BLE-sized uncompressed upload, fast refresh, no etag. Only the first DATA
    # frame is kept (835 of them would be 380 KB of hex); the test rebuilds the
    # rest from client-frame.zlib and checks its first frame against this one.
    add("ble_start", c.build_direct_write_start_uncompressed())
    add("ble_data_first", c.build_direct_write_data_command(raw[: c.CHUNK_SIZE]))
    add("ble_end", c.build_direct_write_end_command(1))

    # mDNS: what python-zeroconf (the client's and Home Assistant's resolver)
    # sends to browse for the type and to resolve one instance. The second
    # packet carries SRV, TXT and A questions, with zeroconf's name compression.
    import zeroconf.const as zc
    from zeroconf import DNSOutgoing, DNSQuestion

    browse = DNSOutgoing(zc._FLAGS_QR_QUERY, multicast=True)
    browse.add_question(DNSQuestion("_opendisplay._tcp.local.", zc._TYPE_PTR, zc._CLASS_IN))
    add("mdns_browse", browse.packets()[0])
    resolve = DNSOutgoing(zc._FLAGS_QR_QUERY, multicast=True)
    instance = "ODCAED44._opendisplay._tcp.local."
    resolve.add_question(DNSQuestion(instance, zc._TYPE_SRV, zc._CLASS_IN))
    resolve.add_question(DNSQuestion(instance, zc._TYPE_TXT, zc._CLASS_IN))
    resolve.add_question(DNSQuestion("ODCAED44.local.", zc._TYPE_A, zc._CLASS_IN))
    add("mdns_resolve", resolve.packets()[0])

    (HERE / "client-vectors.txt").write_text("\n".join(lines) + "\n")
    (HERE / "client-frame.zlib").write_bytes(zlib.compress(raw, 9))
    print(f"{len(lines)} frames, compressed image {len(compressed)} bytes")


if __name__ == "__main__":
    main()
