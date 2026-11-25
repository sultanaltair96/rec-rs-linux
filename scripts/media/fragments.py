#!/usr/bin/env python3
"""Snapshot complete MP4 fragments, or assert they survived a disk fault."""
import hashlib
import json
import struct
import sys
from pathlib import Path


def fragments(path):
    data = Path(path).read_bytes()
    pos = 0
    pending = None
    result = []
    end = 0
    while pos + 8 <= len(data):
        size, kind = struct.unpack_from('>I4s', data, pos)
        header = 8
        if size == 1:
            if pos + 16 > len(data):
                break
            size = struct.unpack_from('>Q', data, pos + 8)[0]
            header = 16
        if size < header or pos + size > len(data):
            break
        if kind == b'moof':
            pending = pos
        elif kind == b'mdat' and pending is not None:
            end = pos + size
            result.append(hashlib.sha256(data[pending:end]).hexdigest())
            pending = None
        pos += size
    return data, result, end


def main():
    mode, source, manifest = sys.argv[1:4]
    data, hashes, end = fragments(source)
    if not hashes:
        raise ValueError('no complete moof/mdat pair yet')
    if mode == 'snapshot':
        Path(manifest).write_text(json.dumps(hashes))
    elif mode == 'verify':
        before = json.loads(Path(manifest).read_text())
        if hashes[:len(before)] != before:
            raise ValueError('previously completed fragments changed or disappeared')
        # Exclude a partial trailing fragment: decoding must prove actual recovery.
        Path(sys.argv[4]).write_bytes(data[:end])
        print(f'preserved {len(before)} original fragments; recovered {len(hashes)}')
    else:
        raise ValueError(f'unknown mode: {mode}')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
