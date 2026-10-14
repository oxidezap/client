"""Check packaged native executables without running foreign code."""
import gzip
import io
import struct
import sys
import tarfile
import zipfile


def machine(data, platform):
    if platform.startswith('linux-'):
        assert data[:6] == b'\x7fELF\x02\x01', 'expected little-endian ELF64'
        assert struct.unpack_from('<H', data, 18)[0] == (183 if platform.endswith('aarch64') else 62), 'wrong ELF architecture'
        assert struct.unpack_from('<H', data, 16)[0] in (2, 3), 'not an executable ELF'
    elif platform == 'macos-aarch64':
        assert data[:4] == b'\xcf\xfa\xed\xfe', 'expected Mach-O 64'
        assert struct.unpack_from('<I', data, 4)[0] == 0x100000C, 'wrong Mach-O architecture'
        assert struct.unpack_from('<I', data, 12)[0] == 2, 'not a Mach-O executable'
    else:
        assert platform == 'windows-x86_64'
        assert data[:2] == b'MZ', 'expected PE executable'
        offset = struct.unpack_from('<I', data, 60)[0]
        assert data[offset:offset + 4] == b'PE\0\0', 'invalid PE signature'
        assert struct.unpack_from('<H', data, offset + 4)[0] == 0x8664, 'wrong PE architecture'


def check(path, version, platform):
    root = f'oxidezap-{version}-{platform}'
    windows = platform.startswith('windows-')
    suffix = '.zip' if windows else '.tar.gz'
    assert path.name == root + suffix, 'wrong archive name'
    if windows:
        with zipfile.ZipFile(path) as archive:
            assert archive.testzip() is None, 'archive CRC failure'
            names = [entry.filename for entry in archive.infolist()]
            assert len(names) == len(set(names)), 'duplicate ZIP member'
            files = {entry.filename: (archive.read(entry), entry.external_attr >> 16)
                     for entry in archive.infolist() if not entry.is_dir()}
    else:
        # Read through the gzip trailer so CRC and truncation errors surface.
        with gzip.open(path, 'rb') as compressed:
            payload = compressed.read()
        with tarfile.open(fileobj=io.BytesIO(payload), mode='r:') as archive:
            files = {}
            for entry in archive:
                if entry.isdir():
                    assert entry.name == root, 'unexpected directory'
                    continue
                assert entry.isfile(), 'links and special files are forbidden'
                assert entry.name not in files, 'duplicate member'
                files[entry.name] = (archive.extractfile(entry).read(), entry.mode)
    binaries = [f'{root}/{name}' + ('.exe' if windows else '')
                for name in ('oxidezap', 'oxidezapd', 'oxidezap-cli')]
    expected = set(binaries)
    if platform.startswith('linux-'):
        expected.update(f'{root}/{name}' for name in ('install-desktop.sh', 'org.oxidezap.client.local.png'))
        assert files[f'{root}/install-desktop.sh'][1] & 0o111, 'installer not executable'
        assert files[f'{root}/org.oxidezap.client.local.png'][0].startswith(b'\x89PNG\r\n\x1a\n'), 'invalid desktop icon'
    assert set(files) == expected, f'unexpected archive contents: {set(files) ^ expected}'
    for name in binaries:
        data, mode = files[name]
        machine(data, platform)
        assert windows or mode & 0o111, 'binary not executable'
    print(f'Validated {path.name}: contents, executable format, architecture and modes')


if __name__ == '__main__':
    from pathlib import Path
    check(Path(sys.argv[1]), sys.argv[2], sys.argv[3])
