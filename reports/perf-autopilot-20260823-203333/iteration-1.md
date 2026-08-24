# Performance autopilot iteration 1

Decision: **REJECT**

## Fresh comparison

Best A: 28,550.044 ms
Candidate A: 35,392.421 ms
Candidate B: not collected
Best B: not collected

Best representative: not available
Candidate representative: not available
Improvement: not available

## Environment comparison

BEST A: sourceFiles=66259, processedFiles=66259, indexedFiles=66259, bytesRead=3049848764
CANDIDATE A: sourceFiles=66259, processedFiles=66259, indexedFiles=66259, bytesRead=3049848764
CANDIDATE B: not collected
BEST B: not collected

## Byte identity

bestTreeSha256: 7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8
candidateTreeSha256: 7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8
identical: True
reason: relative path, file size, and SHA-256 all match

## Decision confidence

sample count: 2
measurement consistency: Candidate A environment and tree match BEST A.
noise observed: none; Candidate A is clearly below the 3% threshold before paired confirmation.
reason: REJECT_PRIMARY_SCORE_A=-23.966% (< 2.5% clear-reject boundary)

## Learning

This hypothesis did not improve the primary score enough to justify a second paired sample.
