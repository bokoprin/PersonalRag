# Performance autopilot iteration 5

Decision: **REJECT**

## Fresh comparison

Best A: 64,700.383 ms
Candidate A: 40,352.619 ms
Candidate B: 49,917.342 ms
Best B: 34,549.716 ms

Best representative: 49,625.050 ms
Candidate representative: 45,134.981 ms
Improvement: 9.048%

## Environment comparison

BEST A: sourceFiles=66259, processedFiles=66259, indexedFiles=66259, bytesRead=3049848764
CANDIDATE A: sourceFiles=66259, processedFiles=66259, indexedFiles=66259, bytesRead=3049848764
CANDIDATE B: sourceFiles=66259, processedFiles=66259, indexedFiles=66259, bytesRead=3049848764
BEST B: sourceFiles=66259, processedFiles=66259, indexedFiles=66259, bytesRead=3049848764

## Byte identity

bestTreeSha256: 7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8
candidateTreeSha256: 7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8
identical: True
reason: relative path, file size, and SHA-256 all match

## Decision confidence

sample count: 4
measurement consistency: BEST A/B each match canonical current-best; candidate A/B each match their fresh BEST environment and byte tree.
noise observed: none
reason: REJECT_PAIRED: representative=9.048% pairAWin=True pairBWin=False

## Learning

The candidate did not beat current-best in both fresh pairs by the required representative threshold.
