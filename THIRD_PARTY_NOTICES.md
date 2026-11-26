# Third-Party Notices

This project incorporates and adapts portions of the following third-party open-source software:

## `scarletkc/colab-playground`

- **Repository**: <https://github.com/scarletkc/colab-playground>
- **Upstream Commit**: `6aa3f8b04c4e79bb086d071cd2929d38df652bf7`
- **Copyright**: Copyright (c) 2026 ScarletKc
- **License**: MIT License
- **Adapted / Reused Files**:
  - `assets/watchdog.py` (adapted from `crates/core/assets/watchdog.py`)
  - `assets/plugins/llama-cpp/plugin.toml` (manifest schema and defaults adapted from `colab-playground` plugin manifest conventions)

```text
MIT License

Copyright (c) 2026 ScarletKc

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## External Tools (Not Bundled)

- **Google Colab CLI (`google-colab-cli`)**: Invoked at runtime from the user's local environment; not redistributed with this package. Use of Google Colab is governed by Google's Terms of Service and Colab Pro/Pro+ policies.
