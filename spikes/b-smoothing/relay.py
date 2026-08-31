#!/usr/bin/env python3
"""UDP relay for the real-link feel test with only one Linux machine.

Run this on the macbook; run sender AND receiver on the Linux box, with the
sender pointed at the mac. Every motion frame then crosses the WiFi twice
(linux -> mac -> linux), so the locally injected cursor experiences the real
radio jitter, doubled. You sit at the Linux machine, move the mouse, and feel
the modes with genuine spikes.

  mac:    python3 relay.py 5556 <linux-ip>:5555
  linux:  ./receiver --port 5555 --mode adaptive
  linux:  ./sender <mac-ip>:5556 --grab
"""

import socket
import sys


def main() -> None:
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    listen_port = int(sys.argv[1])
    host, port = sys.argv[2].rsplit(":", 1)
    dest = (host, int(port))

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("0.0.0.0", listen_port))
    print(f"relaying :{listen_port} -> {dest[0]}:{dest[1]}")
    n = 0
    while True:
        data, _ = sock.recvfrom(2048)
        sock.sendto(data, dest)
        n += 1
        if n % 5000 == 0:
            print(f"{n} frames relayed")


if __name__ == "__main__":
    main()
