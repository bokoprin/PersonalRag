# Filename corpus generator

`CorpusGenerator` emits the canonical `FRCAN001` record stream used by every
filename route. Calibration and official corpora use the seeds and logical
sizes in the specification. The last record size is adjusted so the declared
logical byte total is exact. Generation uses no filesystem content and is
safe to run into a new directory.
