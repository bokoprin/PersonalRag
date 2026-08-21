#!/usr/bin/env python3
import argparse
import html
import random
import shutil
import zipfile
from pathlib import Path


def payload_for(file_index: int, target_bytes: int) -> str:
    lines = []
    total = 0
    i = 0
    while True:
        line = (
            f"file{file_index:05d} line{i:05d} alpha bravo charlie delta echo "
            f"searchable marker{file_index % 97:02d} value{i % 101:03d}\n"
        )
        if total + len(line) > target_bytes:
            break
        lines.append(line)
        total += len(line)
        i += 1
    text = ''.join(lines)
    if len(text) < target_bytes:
        remain = target_bytes - len(text)
        text += ('x' * max(0, remain - 1)) + ('\n' if remain else '')
    return text[:target_bytes]


def chunks_by_parts(lines, parts):
    out = [[] for _ in range(parts)]
    for i, line in enumerate(lines):
        out[i % parts].append(line)
    return [''.join(x) for x in out]


def docx_xml(text: str) -> bytes:
    paras = ''.join(
        f'<w:p><w:r><w:t xml:space="preserve">{html.escape(line)}</w:t></w:r></w:p>'
        for line in text.splitlines(keepends=True)
    )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        f'<w:body>{paras}</w:body></w:document>'
    ).encode()


def docx_aux_xml(text: str, root: str) -> bytes:
    paras = ''.join(
        f'<w:p><w:r><w:t xml:space="preserve">{html.escape(line)}</w:t></w:r></w:p>'
        for line in text.splitlines(keepends=True)
    )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        f'<w:{root} xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        f'{paras}</w:{root}>'
    ).encode()


def xlsx_sheet_xml(text: str) -> bytes:
    rows = []
    for i, line in enumerate(text.splitlines(keepends=True), 1):
        rows.append(
            f'<row r="{i}"><c r="A{i}" t="inlineStr"><is><t xml:space="preserve">'
            f'{html.escape(line)}</t></is></c></row>'
        )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>'
        + ''.join(rows)
        + '</sheetData></worksheet>'
    ).encode()


def pptx_slide_xml(text: str) -> bytes:
    paras = ''.join(
        '<a:p><a:r><a:rPr/><a:t>' + html.escape(line) + '</a:t></a:r></a:p>'
        for line in text.splitlines(keepends=True)
    )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" '
        'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">'
        f'<p:cSld><p:spTree><p:sp><p:txBody>{paras}</p:txBody></p:sp></p:spTree></p:cSld></p:sld>'
    ).encode()


def write_zip(path: Path, entries):
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=6) as z:
        # A few ignored package entries make the central directory less synthetic.
        z.writestr('[Content_Types].xml', '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>')
        z.writestr('_rels/.rels', '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>')
        for name, data in entries:
            z.writestr(name, data)


def media_payload(file_index: int, media_bytes: int) -> bytes:
    if media_bytes <= 0:
        return b''
    return random.Random(0x50455253 + file_index).randbytes(media_bytes)


def write_docx(path: Path, payload: str, multipart: bool, media: bytes = b''):
    if not multipart:
        entries = [('word/document.xml', docx_xml(payload))]
    else:
        parts = chunks_by_parts(payload.splitlines(keepends=True), 6)
        entries = [
            ('word/document.xml', docx_xml(parts[0])),
            ('word/header1.xml', docx_aux_xml(parts[1], 'hdr')),
            ('word/footer1.xml', docx_aux_xml(parts[2], 'ftr')),
            ('word/comments.xml', docx_aux_xml(parts[3], 'comments')),
            ('word/footnotes.xml', docx_aux_xml(parts[4], 'footnotes')),
            ('word/endnotes.xml', docx_aux_xml(parts[5], 'endnotes')),
        ]
    if media:
        entries.append(('word/media/image1.bin', media))
    write_zip(path, entries)


def write_xlsx(path: Path, payload: str, multipart: bool, media: bytes = b''):
    parts = chunks_by_parts(payload.splitlines(keepends=True), 8 if multipart else 1)
    entries = [(f'xl/worksheets/sheet{i+1}.xml', xlsx_sheet_xml(part)) for i, part in enumerate(parts)]
    if media:
        entries.append(('xl/media/image1.bin', media))
    write_zip(path, entries)


def write_pptx(path: Path, payload: str, multipart: bool, media: bytes = b''):
    if not multipart:
        entries = [('ppt/slides/slide1.xml', pptx_slide_xml(payload))]
    else:
        parts = chunks_by_parts(payload.splitlines(keepends=True), 20)
        entries = []
        for i in range(10):
            entries.append((f'ppt/slides/slide{i+1}.xml', pptx_slide_xml(parts[i])))
            entries.append((f'ppt/notesSlides/notesSlide{i+1}.xml', pptx_slide_xml(parts[i+10])))
    if media:
        entries.append(('ppt/media/image1.bin', media))
    write_zip(path, entries)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('root', type=Path)
    ap.add_argument('--files', type=int, default=200)
    ap.add_argument('--payload-bytes', type=int, default=24576)
    ap.add_argument('--media-bytes', type=int, default=0)
    args = ap.parse_args()
    if args.root.exists():
        shutil.rmtree(args.root)
    for profile in ('single', 'multipart'):
        multipart = profile == 'multipart'
        for fmt in ('txt', 'docx', 'xlsx', 'pptx'):
            (args.root / profile / fmt).mkdir(parents=True, exist_ok=True)
        for i in range(args.files):
            payload = payload_for(i, args.payload_bytes)
            media = media_payload(i, args.media_bytes)
            (args.root / profile / 'txt' / f'doc{i:05d}.txt').write_text(payload, encoding='utf-8', newline='')
            write_docx(args.root / profile / 'docx' / f'doc{i:05d}.docx', payload, multipart, media)
            write_xlsx(args.root / profile / 'xlsx' / f'doc{i:05d}.xlsx', payload, multipart, media)
            write_pptx(args.root / profile / 'pptx' / f'doc{i:05d}.pptx', payload, multipart, media)
    print(f'CORPUS_READY root={args.root} files_per_format={args.files} payload_bytes={args.payload_bytes} media_bytes={args.media_bytes}')

if __name__ == '__main__':
    main()
