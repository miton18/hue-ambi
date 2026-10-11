# -*- coding: utf-8 -*-
"""Menu contextuel "Lire sans ambilight" : marque la prochaine lecture, puis la lance.

La marque est une propriete de la fenetre Home (synchrone, contrairement a une notification),
lue par le service au demarrage de la lecture.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import xbmc  # noqa: E402
import xbmcgui  # noqa: E402

from hueambi import SKIP_NEXT  # noqa: E402

if __name__ == '__main__':
    item = sys.listitem
    xbmcgui.Window(10000).setProperty(SKIP_NEXT, str(time.time()))
    xbmc.Player().play(item.getPath(), item)
