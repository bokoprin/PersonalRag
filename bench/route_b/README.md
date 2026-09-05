# Route B

Route B keeps sorted trigram postings for folded filename and full path
representations. Queries with at least one literal run of three code points use
posting intersection followed by exact verification. One and two code point
queries, case sensitive requests, and patterns without a usable literal run
fall back to a complete scan. Updates remain in an overlay and are scan
verified until the next persisted rebuild.
