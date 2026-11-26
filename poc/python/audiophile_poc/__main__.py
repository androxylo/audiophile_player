# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""Entry point for `python -m audiophile_poc`."""

import sys

from .cli import main

if __name__ == "__main__":
    sys.exit(main())
