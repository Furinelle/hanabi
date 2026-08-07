# Third-party notices

Hanabi's optional `douyin_user` source invokes, without vendoring or modifying,
[`jiji262/douyin-downloader`](https://github.com/jiji262/douyin-downloader) at
runtime. The Docker image pins commit
`ad7338fdc474c14e2063370540a362cd8a953b43`. That project is distributed under
the MIT License; its own signing modules retain their upstream Apache-2.0
notices.

The integration uses a subprocess/JSON boundary. Hanabi does not copy the
upstream signing implementation into the Rust binary.
