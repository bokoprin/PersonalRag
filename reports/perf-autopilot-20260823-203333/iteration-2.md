# Performance autopilot iteration 2

Decision: **ACCEPT**

## Fresh comparison

Best A: 33,606.050 ms
Candidate A: 32,651.321 ms
Candidate B: 30,506.288 ms
Best B: 32,315.098 ms

Best representative: 32,960.574 ms
Candidate representative: 31,578.804 ms
Improvement: 4.192%

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
noise observed: Candidate A is within 0.5 percentage points of the threshold; paired B was required.
reason: Both fresh pairs won, the arithmetic representative improved by at least 3%, and every required gate passed.

## Learning

Iteration 1 rejected the display-clone hypothesis. Fresh BEST A shows segment content gram/posting work as the actionable non-protected path.
