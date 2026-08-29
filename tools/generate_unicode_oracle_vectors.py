#!/usr/bin/env python3
import random
import unicodedata as ud
from pathlib import Path

OUT = Path(__file__).resolve().parents[1] / 'tests' / 'unicode_oracle_vectors.txt'

def enc(s: str) -> str:
    return s.encode('utf-8').hex()

vectors = [
    'é', 'e\u0301', 'Straße', 'Σςσ', 'ＡA', '\u1100\u1161',
    'क़', 'ক্ষ', '日本語 ABC', 'İIıi', 'ÅA\u030A', '가\u11A8',
]
# Direct composition pairs and case-fold-sensitive scalars sampled deterministically.
for cp in range(0x110000):
    ch = chr(cp)
    d = ud.decomposition(ch)
    if d and not d.startswith('<'):
        seq = [int(x, 16) for x in d.split()]
        if len(seq) == 2 and len(vectors) < 180:
            vectors.append(''.join(map(chr, seq)))
    if ch.casefold() != ch and cp % 37 == 0 and len(vectors) < 260:
        vectors.append(ch + 'X')

rng = random.Random(0x50525632)
interesting = [
    cp for cp in range(0x20, 0x3000)
    if not (0xD800 <= cp <= 0xDFFF) and chr(cp).isprintable()
]
for _ in range(300):
    length = rng.randint(1, 6)
    vectors.append(''.join(chr(rng.choice(interesting)) for _ in range(length)))

seen = set()
lines = []
for value in vectors:
    if value in seen:
        continue
    seen.add(value)
    nfc = ud.normalize('NFC', value)
    folded = ud.normalize('NFC', nfc.casefold())
    lines.append('|'.join([enc(value), enc(nfc), enc(folded)]))
OUT.write_text('\n'.join(lines) + '\n', encoding='ascii')
print('unicode', ud.unidata_version, 'vectors', len(lines), 'path', OUT)
