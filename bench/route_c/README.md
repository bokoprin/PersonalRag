# Route C

Route C chooses representation from corpus statistics: rare trigrams use
delta encoded postings, medium frequency trigrams use fixed universe bitmaps,
and very common trigrams are omitted so the runtime falls back to a scan. At
query time it estimates the candidate set from available postings and chooses
posting intersection or a scan using the corpus size, then always performs
exact verification. No benchmark query or FileId is special cased.
