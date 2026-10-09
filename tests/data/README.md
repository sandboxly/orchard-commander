RAR samples from the rarfile project (https://github.com/markokr/rarfile,
`test/files/`, ISC license). RAR archives can't be created without the
proprietary `rar` tool, so these are checked in.

- `rar5-solid.rar` — RAR5, solid: stest1.txt, stest2.txt (2048 bytes each)
- `seektest.rar` — RAR4, not solid: same files
- `rar3-solid.rar` — RAR4, solid: bsdtar can't decompress these

`tone-then-silence.mp3` — 1 s of a 440 Hz tone at half volume, then 1 s of
silence (made with ffmpeg; the tests can't encode MP3). Qt's decoder hangs on
MP3s when asked to resample them, which WAV/FLAC files don't show.
