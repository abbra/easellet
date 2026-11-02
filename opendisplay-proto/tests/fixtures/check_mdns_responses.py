#!/usr/bin/env python3
"""Check the Rust mDNS responder's packets with python-zeroconf.

python-zeroconf is what py-opendisplay's LAN discovery and Home Assistant use to
find the frame. tests/client.rs writes "label hex" lines (answers to zeroconf's
own browse and resolve queries, plus an announcement and a goodbye); this
script parses them with zeroconf's DNSIncoming and checks every record a
resolver needs. Usage:

    .tools/venv-opendisplay/bin/python check_mdns_responses.py RESPONSES
"""

import sys

import zeroconf.const as zc
from zeroconf import DNSAddress, DNSIncoming, DNSPointer, DNSService, DNSText

TYPE = "_opendisplay._tcp.local."
INSTANCE = "ODCAED44." + TYPE
HOST = "ODCAED44.local."


def txt_properties(text: bytes) -> dict[bytes, bytes]:
    props = {}
    i = 0
    while i < len(text):
        n = text[i]
        key, _, value = text[i + 1 : i + 1 + n].partition(b"=")
        props[key] = value
        i += 1 + n
    return props


def check(label: str, packet: bytes) -> None:
    message = DNSIncoming(packet)
    assert message.valid, label
    assert message.is_response(), label
    records = message.answers()
    by_type = {type(record): record for record in records}

    if label == "mdns_goodbye":
        assert all(record.ttl == 0 for record in records), label
        return

    # The resolve query asks for SRV, TXT and A only, so it gets no PTR.
    if label == "mdns_resolve":
        assert DNSPointer not in by_type, label
    else:
        ptr = by_type[DNSPointer]
        assert ptr.name.lower() == TYPE and ptr.alias.lower() == INSTANCE.lower(), ptr
    srv = by_type[DNSService]
    assert srv.name.lower() == INSTANCE.lower(), srv
    assert (srv.port, srv.server.lower()) == (2446, HOST.lower()), srv
    assert srv.unique, "SRV must carry the cache-flush bit"
    txt = by_type[DNSText]
    props = txt_properties(txt.text)
    assert props[b"mac"] == b"94:a9:90:ca:ed:46" and props[b"tls"] == b"0", props
    address = by_type[DNSAddress]
    assert address.name.lower() == HOST.lower(), address
    assert address.address == bytes([192, 168, 50, 204]), address
    assert address.type == zc._TYPE_A, address


def main(path: str) -> None:
    seen = []
    with open(path) as responses:
        for line in responses:
            label, data = line.split()
            check(label, bytes.fromhex(data))
            seen.append(label)
    assert seen == ["mdns_browse", "mdns_resolve", "mdns_announce", "mdns_goodbye"], seen
    print(f"zeroconf accepted {len(seen)} mDNS packets")


if __name__ == "__main__":
    main(sys.argv[1])
