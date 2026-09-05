# Independent filename oracle

The oracle scans every `FileRecord` and applies its own NFC, Unicode default
case fold, whitespace-AND, literal, and code-point wildcard semantics. It does
not call a route matcher. Results are recorded as count plus SHA-256 of the
ascending FileId list so every route can be checked for false positives and
false negatives without embedding expected IDs in production code.
