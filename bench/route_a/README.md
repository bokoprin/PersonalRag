# Route A

Route A is the baseline implementation for the filename bake-off. It stores
NFC and folded filename/path strings in contiguous character blobs and scans
every active record for every request. It has no trigram or substring posting
index. Updates are kept in a small overlay so metadata changes remain cheap;
the overlay is materialized when the store is saved.
