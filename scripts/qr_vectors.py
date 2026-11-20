"""Print the reference fingerprints used by the tests in src/qr.rs.

Needs segno (pip install segno==1.6.6). segno 1.6.6 adds a whole zero byte when the bit
stream already ends on a byte boundary; ISO/IEC 18004 7.4.10 only pads up to the boundary,
so that one function is replaced here.
"""

import segno
from segno import encoder


def write_padding_bits(buff, version, length):
    if length % 8:
        buff.extend([0] * (8 - length % 8))


encoder.write_padding_bits = write_padding_bits


def fingerprint(q):
    # FNV-1a over the modules as '0'/'1', row by row
    h = 0xCBF29CE484222325
    for row in q.matrix:
        for dark in row:
            h ^= ord("1") if dark else ord("0")
            h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"


def text(n):
    return "".join(ALPHABET[(i * 37 + n * 11 + (i * i) % 29) % 64] for i in range(n))


def make(data, **kw):
    return segno.make(data, mode="byte", boost_error=False, micro=False, **kw)


for level in "LM":
    prints = []
    for v in range(1, 41):
        q = make(b"ridgewarden", error=level, version=v, mask=v % 8)
        prints.append("0x%016x" % fingerprint(q))
    print("const FIXED_%s: [u64; 40] = [%s];" % (level, ", ".join(prints)))

rows = []
for n in [1, 14, 15, 40, 100, 300, 700, 1200, 1800, 2331, 2332, 2900]:
    data = text(n).encode()
    try:
        q = make(data, error="M")
    except segno.DataOverflowError:
        q = make(data, error="L")
    rows.append("(%d, %d, Level::%s, %d, 0x%016x)" % (n, q.version, q.error, q.mask, fingerprint(q)))
print("const AUTO: [(usize, u8, Level, u8, u64); %d] = [%s];" % (len(rows), ", ".join(rows)))
