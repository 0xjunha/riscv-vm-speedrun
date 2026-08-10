# littlefs upstream provenance

- Upstream: [`littlefs-project/littlefs`](https://github.com/littlefs-project/littlefs)
- Revision: tag `v2.11.3`, commit `6cb4e86540eca0d9ba62500a298385c9d863c8be`
- Imported files:
  - `lfs.c`: SHA-256 `a36d6a095785ddea9571d541d68d3e4ef01d5b255a99d17d3f07fb6ea60ea132`
  - `lfs.h`: SHA-256 `b1befd7288d08815accc8f9af744c55686c0b9e3ac0061c32ceee38a1b3eb96d`
  - `lfs_util.c`: SHA-256 `f2fbde533670560434bd9f5a547174cc7c5a4670a02c47b4bd85180dced8b2ec`
  - `lfs_util.h`: SHA-256 `548d46aa524dc7449e16739286c1a422a52f9de727ff0be0c2ffc5593f5ca981`
- Relationship: byte-for-byte vendored upstream filesystem sources. Project-authored benchmark integration and block-device callbacks live outside this directory.
- License: [BSD 3-Clause](../../guest/licenses/littlefs-BSD-3-Clause.txt).
