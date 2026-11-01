#!/usr/bin/python3 -I
# Prints "<name> <version> <release> <arch>" of a package file, for verify-assets.sh to check the inner
# identifiers (the verify job has no rpm or dpkg tools):
#   package-info.py rpm kuickshare-1.7.0-1.fc45.x86_64.rpm    -> kuickshare 1.7.0 1.fc45 x86_64
#   package-info.py deb kuickshare_1.7.0-1_amd64.deb          -> kuickshare 1.7.0-1 - amd64
#   package-info.py pkg kuickshare-1.7.0-1-x86_64.pkg.tar.zst -> kuickshare 1.7.0-1 - x86_64
# Reads only what the formats define: an rpm lead and its two header sections (signature, package),
# a deb ar archive and its control.tar member, a pkg tar (zstd, decompress with the zstd binary) and
# its .PKGINFO text. One line on success, exit 0; a clear message and non-zero on anything else.
# Usage: package-info.py {rpm|deb|pkg} <package file>

import io
import os
import struct
import subprocess
import sys
import tarfile


def fail(msg):
    sys.exit("package-info: %s" % msg)


def text_of(tar, member):
    return tar.extractfile(member).read().decode("utf-8", "replace")


# ---- rpm ----
# The lead: 96 bytes of magic \xed\xab\xee\xdb and preload, at offset 0. After it, two header sections
# back to back and in total 8-byte aligned: an 8-byte header magic (8E AD E8 + version), 4 reserve
# bytes, count and size (int32 big endian), then count 16-byte index records (tag, kind, offset, count)
# and size bytes of a data section. NAME=1000, VERSION=1001, RELEASE=1002 and ARCH=1020 hold one
# zero-ended string (STRING, 6) whose offset lands in that section.
RPM_STRING = 6
RPM_STRINGS = {"name": 1000, "version": 1001, "release": 1002, "arch": 1022}


def rpm_info(path):
    with open(path, "rb") as f:
        lead = f.read(96)
        if len(lead) < 96 or lead[:4] != b"\xed\xab\xee\xdb":
            fail("%s: not a valid rpm (bad lead)" % path)

        def read_header():
            head = f.read(16)
            if len(head) < 16 or head[:3] != b"\x8e\xad\xe8":
                fail("%s: not a valid rpm (no header magic)" % path)
            count, size = struct.unpack(">II", head[8:16])
            entries = [struct.unpack(">IIII", f.read(16)) for _ in range(count)]
            data = f.read(size)
            if len(data) < size:
                fail("%s: not a valid rpm (truncated header data)" % path)
            at = f.tell()
            if at % 8:
                f.seek(8 - at % 8, 1)  # header sections are 8-byte aligned
            return entries, data

        entries, data = read_header()
        entries, data = read_header()

        fields = {}
        for tag, kind, offset, count in entries:
            names = [n for n, t in RPM_STRINGS.items() if t == tag]
            if kind != RPM_STRING or not names:
                continue
            if offset >= len(data):
                fail("%s: rpm tag %s does not hold one string" % (path, names[0]))
            stop = data.find(b"\0", offset)
            if stop < 0 or count != 1:
                fail("%s: rpm tag %s does not hold one string" % (path, names[0]))
            fields[names[0]] = data[offset:stop].decode("utf-8", "replace")
        if set(fields) != set(RPM_STRINGS):
            fail("%s: the rpm header misses %s" % (path, sorted(set(RPM_STRINGS) - set(fields))))
        # Release order: name version release arch.
        return fields["name"], fields["version"], fields["release"], fields["arch"]


# ---- deb ----
# A Unix ar: the "established members" debian-binary, control.tar.X (the control file, in a container
# tarfile knows: gz, bz2, xz; plain also works) and data.tar.X. Member names end in '/', long names
# (from a GNU ar "#1/len" member) can prefix the size with the name bytes themselves.
def deb_info(path):
    with open(path, "rb") as f:
        if f.read(8) != b"!<arch>\n":
            fail("%s: not a valid deb (no ar magic)" % path)
        control = None
        while True:
            head = f.read(60)
            if len(head) < 60:
                break
            name = head[0:16].decode("ascii", "replace").rstrip().rstrip("/")
            size = int(head[48:58])
            if name.startswith("#1/"):
                extra = f.read(int(name[3:]))
                name, size = extra.rstrip(b"\0").decode("ascii", "replace"), size - len(extra)
            if name.startswith("control.tar"):
                control = f.read(size)
                f.seek(size & 1, 1)  # members are padded to a 2-byte boundary
                break
            f.seek(size + (size & 1), 1)
        if control is None:
            fail("%s: not a valid deb (no control.tar member)" % path)

    tar_bytes = control
    if control[:4] == b"\x28\xb5\x2f\xfd":  # zstd, which tarfile does not know
        ran = subprocess.run(["zstd", "-dc"], input=control, stdout=subprocess.PIPE)
        if ran.returncode != 0:
            fail("%s: zstd could not decompress control.tar" % path)
        tar_bytes = ran.stdout
    try:
        t = tarfile.open(fileobj=io.BytesIO(tar_bytes), mode="r:*")
    except tarfile.ReadError as e:
        fail("%s: control.tar is not a readable tar: %s" % (path, e))
    for member in t.getmembers():
        if member.name.split("/")[-1] == "control":
            fields = deb_control_fields(text_of(t, member), path)
            # name, version, arch: deb control carries the Debian revision in Version.
            return fields[0], fields[1], fields[2]
    fail("%s: control.tar has no control file" % path)


def deb_control_fields(text, path):
    got = {}
    for para in text.split("\n\n"):
        for line in para.splitlines():
            if ": " in line:
                key, _, value = line.partition(": ")
                got[key] = value.strip()
    for key in ("Package", "Version", "Architecture"):
        if not got.get(key):
            fail("%s: the deb control misses %s" % (path, key))
    return got["Package"], got["Version"], got["Architecture"]


# ---- pkg ----
# The package's own tar member, zstd compression (pkg.tar.zst is always zstd for arch), storing
# .PKGINFO as lines of "key = value".
def pkg_info(path):
    with open(path, "rb") as f:
        if f.read(4) != b"\x28\xb5\x2f\xfd":
            fail("%s: not a valid pkg (no zstd magic)" % path)
    ran = subprocess.run(["zstd", "-dc", path], stdout=subprocess.PIPE)
    if ran.returncode != 0:
        fail("%s: zstd could not decompress it" % path)
    try:
        t = tarfile.open(fileobj=io.BytesIO(ran.stdout), mode="r:")
    except tarfile.ReadError as e:
        fail("%s: not a readable tar after zstd: %s" % (path, e))
    for member in t.getmembers():
        if member.name.split("/")[-1] == ".PKGINFO":
            got = {}
            for line in text_of(t, member).splitlines():
                if " = " in line:
                    key, _, value = line.partition(" = ")
                    got[key] = value.strip()
            for key in ("pkgname", "pkgver", "arch"):
                if not got.get(key):
                    fail("%s: .PKGINFO misses %s" % (path, key))
            return got["pkgname"], got["pkgver"], "-", got["arch"]
    fail("%s: no .PKGINFO member" % path)


def main():
    args = sys.argv[1:]
    if len(args) != 2 or args[0] not in ("rpm", "deb", "pkg"):
        sys.exit("usage: package-info.py {rpm|deb|pkg} <package file>")
    kind, path = args
    if not os.path.isfile(path):
        fail("%s is not a file" % path)
    if kind == "rpm":
        name, version, release, arch = rpm_info(path)
        print("%s %s %s %s" % (name, version, release, arch))
    elif kind == "deb":
        name, version, arch = deb_info(path)
        print("%s %s - %s" % (name, version, arch))
    else:
        print("%s %s %s %s" % pkg_info(path))


if __name__ == "__main__":
    main()
