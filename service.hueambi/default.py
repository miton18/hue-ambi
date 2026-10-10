# -*- coding: utf-8 -*-
"""Point d'entree du service Hue Ambi (convention LibreELEC : le code vit dans resources/lib)."""
import os
import sys

sys.path.append(os.path.join(os.path.dirname(__file__), 'resources', 'lib'))

import hueambi_service  # noqa: E402

hueambi_service.run()
