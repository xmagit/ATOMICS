"""A deterministic upstream resolver in the disposable test network."""
import socket
import struct

peer = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
peer.bind(("0.0.0.0", 53))
while True:
    query, address = peer.recvfrom(4096)
    # Locate QTYPE in the question, before any EDNS OPT record dnsmasq appends.
    end = 12
    while query[end]:
        end += query[end] + 1
    end += 1
    kind = struct.unpack("!H", query[end:end + 2])[0]
    question = query[12:end + 4]
    answer = b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04\xc0\xa8\x01\x02" if kind == 1 else b""
    peer.sendto(query[:2] + b"\x81\x80\x00\x01" + struct.pack("!H", bool(answer)) + b"\x00\x00\x00\x00" + question + answer, address)
