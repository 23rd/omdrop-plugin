#!/usr/bin/env python3
"""Send a file over KaiT2en AWDL with the Omdrop AirDrop sender."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

import dbus


HELPERS = Path('/usr/lib/omdrop-core')
IFACE = 'awdl0'
PORT = 8770
MAC = re.compile(r'(?:[0-9a-f]{2}:){5}[0-9a-f]{2}')


def registered_peers():
    try:
        result = subprocess.run(
            ['ip', '-json', '-6', 'neigh', 'show', 'dev', IFACE],
            capture_output=True, text=True, timeout=1, check=True,
        )
        neighbors = json.loads(result.stdout)
    except (OSError, ValueError, subprocess.SubprocessError):
        return {}
    # KaiT2en installs permanent IPv6 neighbours when it registers the
    # peer in Broadcom's firmware. Use their scoped addresses as supplied.
    return {
        mac: ip for entry in neighbors
        if 'PERMANENT' in entry.get('state', [])
        and MAC.fullmatch(mac := entry.get('lladdr', '').lower())
        and (ip := entry.get('dst', '')).startswith('fe80:')
    }


def probe_peers():
    def discover(peer):
        mac, host = peer
        try:
            result = subprocess.run(
                [str(HELPERS / 'airdrop-send'), '--discover-only',
                 '--direct', f'[{host}%{IFACE}]:{PORT}', '--op-timeout', '2.5'],
                capture_output=True, text=True, timeout=4, check=True,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        name = result.stdout.strip()
        return (mac, name) if name else None

    peers = list(registered_peers().items())[:8]
    with ThreadPoolExecutor(max_workers=8) as pool:
        return [peer for peer in pool.map(discover, peers) if peer is not None]


def find_receiver(sender, name, deadline):
    want = name.casefold()
    while time.monotonic() < deadline:
        peers = [(str(mac), str(label)) for mac, label in sender.Receivers()]
        if MAC.fullmatch(want):
            matches = [(mac, label) for mac, label in peers if mac.casefold() == want]
            if not matches and want in registered_peers():
                matches = [(want, want)]
        else:
            matches = [(mac, label) for mac, label in peers if label.casefold() == want]
            if not matches:
                matches = [(mac, label) for mac, label in peers if want in label.casefold()]
        if len(matches) > 1:
            raise RuntimeError(f'Multiple receivers match {name!r}. Use the full name or AWDL address.')
        if matches:
            mac, label = matches[0]
            octets = bytearray.fromhex(mac.replace(':', ''))
            if len(octets) != 6:
                raise RuntimeError(f'Invalid AWDL address for {label!r}.')
            octets[0] ^= 2
            host = socket.inet_ntop(socket.AF_INET6, b'\xfe\x80' + bytes(6) + octets[:3] + b'\xff\xfe' + octets[3:])
            try:
                with socket.socket(socket.AF_INET6) as connection:
                    connection.settimeout(min(0.8, max(0.01, deadline - time.monotonic())))
                    connection.connect((host, PORT, 0, socket.if_nametoindex(IFACE)))
            except OSError:
                time.sleep(0.2)
                continue
            return host, label
        time.sleep(0.3)
    raise RuntimeError('The recipient did not open its AirDrop receiver. Unlock it and refresh Everyone for 10 Minutes.')


def send_file(upload, host, name, files, gui=False):
    command = [
        str(upload), '--direct', f'[{host}%{IFACE}]:{PORT}',
        '--name', name, '--op-timeout', '30', '--', *map(str, files),
    ]
    # A listener can finish discovery and miss the separate TLS handshake for
    # /Ask. Retry exactly that failure before any consent request goes out.
    for attempt in range(2):
        asked = False
        handshake_timeout = False
        failure = 'The AirDrop connection failed.'
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              text=True, errors='replace') as transfer:
            for line in transfer.stdout:
                if not gui:
                    print(line, end='', file=sys.stderr, flush=True)
                elif 'POST /Ask' in line:
                    print('Waiting for the recipient to accept…', flush=True)
                elif 'ASK -> accepted' in line:
                    print('Accepted. Sending…', flush=True)
                    failure = 'The file upload failed.'
                elif 'ASK -> declined' in line:
                    failure = 'The recipient declined the transfer.'
                elif 'ASK failed:' in line:
                    failure = 'The recipient did not answer the AirDrop request.'
                asked |= 'POST /Ask' in line
                handshake_timeout |= any(text in line.lower() for text in ('timed out', 'would block'))
        if not transfer.returncode or asked or not handshake_timeout or attempt:
            if gui and transfer.returncode:
                print(failure, file=sys.stderr, flush=True)
            return transfer.returncode
        retry = 'The AirDrop connection timed out. Trying again…' if gui else (
            'TLS handshake timed out before the acceptance prompt. Retrying once.'
        )
        print(retry, file=sys.stderr, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--to', required=True, help='name or AWDL address of an Apple device')
    parser.add_argument('--wait', type=int, default=45, help='seconds to wait for the receiver to open')
    parser.add_argument('--name', default=socket.gethostname().partition('.')[0], help='the sender name shown on the Apple device')
    parser.add_argument('--gui', action='store_true', help='print short progress messages for the Omdrop panel')
    parser.add_argument('files', nargs='+', type=Path)
    args = parser.parse_args()
    if args.wait < 1:
        parser.error('--wait must be at least one second')
    for path in args.files:
        if not path.is_file():
            parser.error(f'{path}: expected a file')

    advertiser = HELPERS / 'omdrop-adv'
    upload = HELPERS / 'airdrop-send'
    if not advertiser.is_file() or not upload.is_file():
        parser.error('The Omdrop AirDrop userspace helpers are missing.')

    # Keep the browse client attached until the upload finishes. Otherwise
    # the daemon may stop AWDL while it is still carrying the transfer.
    bus = dbus.SystemBus(private=True)
    try:
        peer_service = bus.get_object('org.t2drop', '/org/t2drop')
        sender = dbus.Interface(peer_service, 'org.t2drop.Sender1')
        with subprocess.Popen([
            str(advertiser), 'ble', '--arm', 'A', '--broadcast',
            '--hashes-from-record', '--seconds', str(args.wait + 300),
        ], stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL if args.gui else None) as wake:
            try:
                sender.Browse()
                host, label = find_receiver(sender, args.to, time.monotonic() + args.wait)
                if not args.gui:
                    print(f'Sending to {label}. Accept the AirDrop prompt on the device.', flush=True)
                return send_file(upload, host, args.name, args.files, gui=args.gui)
            finally:
                if wake.poll() is None:
                    wake.terminate()
    finally:
        bus.close()


if __name__ == '__main__':
    if sys.argv[1:] == ['--probe-peers']:
        print(json.dumps(probe_peers()))
        raise SystemExit(0)
    try:
        raise SystemExit(main())
    except (dbus.DBusException, OSError, RuntimeError) as error:
        raise SystemExit(str(error)) from None
