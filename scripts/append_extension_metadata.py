#!/usr/bin/env python3
"""Append DuckDB extension metadata footer to a raw shared library so it can be
LOADed as a `.duckdb_extension`.

Vendored from duckdb/extension-ci-tools (MIT). For C-API (C_STRUCT ABI)
extensions, `--duckdb-version` encodes the targeted C API version.
"""
import argparse
import shutil


def start_signature():
    encoded_string = ''.encode('ascii')
    encoded_string += int(0).to_bytes(1, byteorder='big')
    encoded_string += int(147).to_bytes(1, byteorder='big')
    encoded_string += int(4).to_bytes(1, byteorder='big')
    encoded_string += int(16).to_bytes(1, byteorder='big')
    encoded_string += b'duckdb_signature'
    encoded_string += int(128).to_bytes(1, byteorder='big')
    encoded_string += int(4).to_bytes(1, byteorder='big')
    return encoded_string


def padded_byte_string(input):
    encoded_string = input.encode('ascii')
    encoded_string += b'\x00' * (32 - len(encoded_string))
    return encoded_string


def main():
    p = argparse.ArgumentParser(description='Append extension metadata to a loadable DuckDB extension')
    p.add_argument('-l', '--library-file', required=True)
    p.add_argument('-n', '--extension-name', required=True)
    p.add_argument('-o', '--out-file', default='')
    p.add_argument('-p', '--duckdb-platform', required=True)
    p.add_argument('-dv', '--duckdb-version', required=True)
    p.add_argument('-ev', '--extension-version', default='0.1.0')
    p.add_argument('--abi-type', default='C_STRUCT')
    args = p.parse_args()

    out = args.out_file if args.out_file else args.extension_name + '.duckdb_extension'
    tmp = out + '.tmp'
    shutil.copyfile(args.library_file, tmp)
    with open(tmp, 'ab') as f:
        f.write(start_signature())
        f.write(padded_byte_string(""))            # FIELD8 unused
        f.write(padded_byte_string(""))            # FIELD7 unused
        f.write(padded_byte_string(""))            # FIELD6 unused
        f.write(padded_byte_string(args.abi_type)) # FIELD5 abi_type
        f.write(padded_byte_string(args.extension_version))  # FIELD4
        f.write(padded_byte_string(args.duckdb_version))     # FIELD3
        f.write(padded_byte_string(args.duckdb_platform))    # FIELD2
        f.write(padded_byte_string("4"))           # FIELD1 header signature
        f.write(b"\x00" * 256)                     # empty signature (unsigned)
    shutil.move(tmp, out)
    print(f"wrote {out}")


if __name__ == '__main__':
    main()
