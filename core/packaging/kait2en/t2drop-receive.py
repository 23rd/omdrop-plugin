#!/usr/bin/env python3
"""AirDrop file delivery for the KaiT2en D-Bus service."""

import argparse
from contextlib import ExitStack
import fcntl
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib, GLibUnix


SERVICE = 'org.t2drop'
RECEIVER = 'org.t2drop.Receiver1'
AGENT = 'org.t2drop.Agent1'
PATH = '/org/t2drop/omdrop'
MAX_FILES = 256
MAX_BYTES = 2 << 30
REQUIRED_SEALS = fcntl.F_SEAL_WRITE | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_GROW


def leaf(name):
    # AirDrop directories can contain paths. Store their files by basename so
    # a remote name never selects a local directory or follows its symlinks.
    name = name.rsplit('/', 1)[-1]
    if name in ('', '.', '..') or '\0' in name or len(os.fsencode(name)) > 255:
        raise ValueError('Invalid AirDrop filename')
    return name


def save_files(directory, names, descriptors):
    if not names or len(names) != len(descriptors) or len(names) > MAX_FILES:
        raise ValueError('Invalid AirDrop file list')
    names = [leaf(name) for name in names]
    with ExitStack() as stack:
        sources = [stack.enter_context(os.fdopen(fd.take(), 'rb')) for fd in descriptors]
        sizes = []
        for source in sources:
            seals = fcntl.fcntl(source, fcntl.F_GET_SEALS)
            if seals & REQUIRED_SEALS != REQUIRED_SEALS:
                raise ValueError('AirDrop payload is not sealed')
            sizes.append(os.fstat(source.fileno()).st_size)
        total = sum(sizes)
        if total > MAX_BYTES or total > shutil.disk_usage(directory).free:
            raise ValueError('AirDrop payload exceeds available space or transfer limit')
        # Stage the whole transfer before publishing any file. A hard link
        # publishes a complete file atomically and never overwrites a name.
        staging = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix='.airdrop-', dir=directory)))
        for index, source in enumerate(sources):
            source.seek(0)
            with (staging / str(index)).open('xb') as target:
                os.fchmod(target.fileno(), 0o600)
                shutil.copyfileobj(source, target)
        saved = []
        try:
            for index, name in enumerate(names):
                stem, ext = os.path.splitext(name)
                stem = os.fsencode(stem)[:160].decode('utf-8', 'ignore')
                ext = os.fsencode(ext)[:72].decode('utf-8', 'ignore')
                for duplicate in range(10000):
                    suffix = f' ({duplicate})' if duplicate else ''
                    filename = name if not suffix else f'{stem[:55]}{suffix}{ext[:40]}'
                    try:
                        os.link(staging / str(index), directory / filename)
                    except FileExistsError:
                        continue
                    saved.append(directory / filename)
                    break
                else:
                    raise OSError('Could not find a free filename')
        except OSError:
            for path in saved:
                path.unlink(missing_ok=True)
            raise
    return saved


class Agent(dbus.service.Object):
    def __init__(self, bus, directory, once, shutdown):
        super().__init__(bus, PATH)
        self.bus = bus
        self.directory = directory
        self.once = once
        self.shutdown = shutdown
        self.pending = {}

    def check_sender(self, caller):
        if caller != self.bus.get_name_owner(SERVICE):
            raise dbus.exceptions.DBusException('Only t2drop can call the AirDrop agent',
                                               name='org.freedesktop.DBus.Error.AccessDenied')

    @dbus.service.method(AGENT, in_signature='ssas', out_signature='b', sender_keyword='caller')
    def Ask(self, transfer, _sender, files, caller=None):
        self.check_sender(caller)
        now = time.monotonic()
        self.pending = {key: expires for key, expires in self.pending.items() if expires > now}
        if not files or len(files) > MAX_FILES or len(self.pending) >= 16 or (self.once and self.pending):
            return False
        # The Omdrop receive switch opens an Everyone window. Accept a valid
        # Ask during that window and bind delivery to this service and ID.
        self.pending[(caller, transfer)] = now + 1800
        return True

    @dbus.service.method(AGENT, in_signature='ssasah', sender_keyword='caller')
    def Deliver(self, transfer, _sender, names, descriptors, caller=None):
        self.check_sender(caller)
        if self.pending.pop((caller, transfer), 0) <= time.monotonic():
            raise dbus.exceptions.DBusException('AirDrop transfer has no matching request',
                                               name='org.freedesktop.DBus.Error.AccessDenied')
        try:
            saved = save_files(self.directory, names, descriptors)
        except (OSError, ValueError) as error:
            print(f'AirDrop could not save files: {error}', file=sys.stderr, flush=True)
            raise dbus.exceptions.DBusException('Could not save the AirDrop transfer') from error
        print(f'AirDrop saved {len(saved)} file(s)', flush=True)
        for path in saved:
            try:
                subprocess.run(
                    ['/usr/lib/omdrop-core/omdrop', 'handle', str(path)],
                    timeout=5, check=False,
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                print(f'AirDrop arrival notification failed: {error}', file=sys.stderr, flush=True)
        if self.once:
            GLib.idle_add(self.shutdown)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--name', required=True)
    parser.add_argument('--dir', required=True, type=Path)
    parser.add_argument('--once', action='store_true')
    args = parser.parse_args()
    directory = args.dir.resolve(strict=True)
    if not directory.is_dir() or not os.access(directory, os.W_OK):
        parser.error(f'{directory} is not a writable folder')
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus(private=True)
    loop = GLib.MainLoop()

    def shutdown(*_):
        loop.quit()
        return False

    agent = Agent(bus, directory, args.once, shutdown)
    owner = None
    receiver = dbus.Interface(bus.get_object(SERVICE, '/org/t2drop'), RECEIVER)
    try:
        receiver.RegisterAgent(dbus.ObjectPath(PATH), args.name)
        receiver.StartReceiving()
        registered_owner = bus.get_name_owner(SERVICE)
        owner = bus.watch_name_owner(SERVICE, lambda name: None if name == registered_owner else shutdown())
        GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, shutdown)
        if os.environ.get('NOTIFY_SOCKET'):
            subprocess.run(['systemd-notify', '--ready'], check=True)
        loop.run()
    finally:
        if owner is not None:
            owner.cancel()
        agent.remove_from_connection()
        bus.close()


if __name__ == '__main__':
    try:
        main()
    except (OSError, dbus.DBusException, subprocess.SubprocessError) as error:
        raise SystemExit(str(error)) from None
