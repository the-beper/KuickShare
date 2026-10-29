#!/usr/bin/env python3
"""Writes the notes of a GitHub release (Markdown, on stdout) from the metainfo file.

usage: release-notes.py METAINFO VERSION [--repo OWNER/NAME]

The notes are "## What's Changed", the items of the lists in the <description> of <release version="VERSION"> as
bullets and, when --repo is given and an older <release> follows in the metainfo (newest first), a "Full Changelog" link
that compares the two tags. The introductory <p> is for the software centers and is not used. Exits 1 when the metainfo
has no such release.
"""
import argparse
import sys
import xml.etree.ElementTree as ET


def text_of(element):
    """The text of an element with its inline children (<code>, <em>) as Markdown."""
    parts = [element.text or ""]
    for child in element:
        inner = text_of(child)
        parts.append("`%s`" % inner if child.tag == "code" else inner)
        parts.append(child.tail or "")
    return " ".join("".join(parts).split())


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[1])
    parser.add_argument("metainfo")
    parser.add_argument("version")
    parser.add_argument("--repo", default="")
    args = parser.parse_args()

    releases = list(ET.parse(args.metainfo).getroot().iter("release"))
    index = next((i for i, r in enumerate(releases) if r.get("version") == args.version), None)
    if index is None:
        print("release-notes: no <release version=\"%s\"> in %s" % (args.version, args.metainfo), file=sys.stderr)
        return 1
    description = releases[index].find("description")
    items = [text_of(li) for li in description.iter("li")] if description is not None else []

    out = "## What's Changed\n\n" + "\n".join("- " + item for item in items or ["KuickShare %s." % args.version])
    if args.repo and index + 1 < len(releases):
        previous = releases[index + 1].get("version")
        out += "\n\n**Full Changelog**: https://github.com/%s/compare/v%s...v%s" % (args.repo, previous, args.version)
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
