#!/usr/bin/env python3
"""Reject a release that cannot use webOS's aarch64 glibc 2.35 loader."""
import pathlib
import struct
import sys


def check(path):
    data = pathlib.Path(path).read_bytes()
    if data[:6] != b'\x7fELF\x02\x01' or struct.unpack_from('<H', data, 18)[0] != 183:
        raise ValueError('expected a little-endian ELF64 aarch64 executable')
    shoff = struct.unpack_from('<Q', data, 40)[0]
    shsize, count, strings_index = struct.unpack_from('<HHH', data, 58)
    sections = [struct.unpack_from('<IIQQQQIIQQ', data, shoff + i * shsize) for i in range(count)]
    section_names = sections[strings_index]
    strings = data[section_names[4]:section_names[4] + section_names[5]]
    def text(buf, offset):
        return buf[offset:buf.index(b'\0', offset)].decode()
    sections = {text(strings, s[0]): s for s in sections}
    dyn = sections['.dynstr']
    dynstr = data[dyn[4]:dyn[4] + dyn[5]]
    versions = []
    section = sections['.gnu.version_r']
    at, end = section[4], section[4] + section[5]
    while at < end:
        _, count, _, aux, next_entry = struct.unpack_from('<HHIII', data, at)
        pos = at + aux
        for _ in range(count):
            _, _, _, name, next_aux = struct.unpack_from('<IHHII', data, pos)
            value = text(dynstr, name)
            if value.startswith('GLIBC_'):
                version = tuple(map(int, value[6:].split('.')))
                if version > (2, 35):
                    raise ValueError(f'{value} exceeds webOS glibc 2.35')
                versions.append(version)
            pos += next_aux
        if not next_entry:
            break
        at += next_entry
    if not versions:
        raise ValueError('no glibc symbol versions found')
    print(f'{path}: ELF64 aarch64; highest glibc requirement {".".join(map(str, max(versions)))}')


if __name__ == '__main__':
    check(sys.argv[1])
