# -*- coding: utf-8 -*-
"""Service Hue Ambi : lance le moteur Rust selon les reglages et le relance s'il s'arrete.

Pendant une lecture, l'ambilight peut etre coupe pour cette lecture seulement : touche de
la telecommande (NotifyAll envoye par le fichier de touches genere) ou menu contextuel
"Lire sans ambilight" (propriete SKIP_NEXT). Le moteur est arrete, puis relance a la fin.
"""
import subprocess
import threading
import time

import xbmc
import xbmcgui

from hueambi import (CONF, ID, REQUIRED, SKIP_NEXT, addon, binary, env, import_old_credentials, log, notify, tr,
                     values, write_conf, write_keymap)

HOME = 10000


class Player(xbmc.Player):
    """Debut et fin des lectures, pour la coupure limitee a une lecture."""

    def __init__(self, svc):
        super().__init__()
        self.svc = svc

    def onAVStarted(self):
        self.svc.playback_started()

    def onPlayBackStopped(self):
        self.svc.playback_ended()

    def onPlayBackEnded(self):
        self.svc.playback_ended()

    def onPlayBackError(self):
        self.svc.playback_ended()


class Service(xbmc.Monitor):
    def __init__(self):
        super().__init__()
        self.proc = None
        self.current = False  # configuration du moteur en cours, None si inactif, False au depart
        self.suspended = False  # ambilight coupe pour la lecture en cours
        self.keymap = None
        self.fails = 0
        self.started = 0.0
        self.next_start = 0.0

    def onSettingsChanged(self):
        self.apply()
        self.update_keymap()

    def onNotification(self, sender, method, data):
        if sender == ID and method.endswith('.toggle'):
            self.toggle()

    def apply(self):
        a = addon()
        v = values(a)
        enabled = a.getSettingBool('enabled')
        want = v if enabled and all(v[k] for k in REQUIRED) else None
        if want == self.current and (want is None or self.proc is not None or self.suspended):
            return
        self.stop()
        write_conf(v)  # sert aussi aux actions (liste des zones)
        self.current = want
        if want is None:
            log('inactif : %s' % ('desactive' if not enabled else 'appairage ou zone manquant'))
            return
        self.fails = 0
        if not self.suspended:
            self.start()

    def update_keymap(self):
        key = addon().getSetting('toggle_key') or 'blue'
        if key == self.keymap:
            return
        self.keymap = key
        try:
            write_keymap(key)
        except OSError as e:
            log('fichier de touches : %s' % e, xbmc.LOGWARNING)
            return
        xbmc.executebuiltin('Action(reloadkeymaps)')
        log('touche de bascule : %s' % key)

    def start(self):
        try:
            self.proc = subprocess.Popen([binary(), 'run'], env=env(CONF), stdin=subprocess.DEVNULL,
                                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        except OSError as e:
            log('lancement impossible : %s' % e, xbmc.LOGERROR)
            self.proc = None
            self.next_start = time.time() + 30
            return
        self.started = time.time()
        threading.Thread(target=self.pump, args=(self.proc,), daemon=True).start()
        log('moteur demarre (pid %d)' % self.proc.pid)

    @staticmethod
    def pump(proc):
        for line in iter(proc.stderr.readline, b''):
            log(line.decode('utf-8', 'replace').rstrip())

    def stop(self):
        p, self.proc = self.proc, None
        if p and p.poll() is None:
            p.terminate()  # le moteur rend la zone au pont avant de quitter
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
            log('moteur arrete')

    def suspend(self):
        self.suspended = True
        self.stop()
        notify(30121)
        log('ambilight coupe pour cette lecture')

    def resume(self, quiet=False):
        self.suspended = False
        log('ambilight reactive')
        if not quiet:
            notify(30122)
        if self.current and self.proc is None:
            self.fails = 0
            self.start()

    def toggle(self):
        if not xbmc.Player().isPlayingVideo():
            log('bascule demandee sans video en cours')
            notify(30123)
            return
        if self.suspended:
            self.resume()
        else:
            self.suspend()

    def playback_started(self):
        home = xbmcgui.Window(HOME)
        asked = home.getProperty(SKIP_NEXT)
        if not asked:
            return
        home.clearProperty(SKIP_NEXT)
        # demande recente seulement : une lecture qui a echoue ne doit pas couper la suivante
        try:
            recent = time.time() - float(asked) < 60
        except ValueError:
            recent = False
        if recent and xbmc.Player().isPlayingVideo() and not self.suspended:
            self.suspend()

    def playback_ended(self):
        if self.suspended:
            self.resume(quiet=True)

    def tick(self):
        if self.current is None or self.suspended:
            return
        if self.proc is None:
            if time.time() >= self.next_start:
                self.start()
            return
        if self.proc.poll() is None:
            if self.fails and time.time() - self.started > 120:
                self.fails = 0
            return
        self.fails += 1
        delay = min(60, 5 * self.fails)
        log('moteur sorti (code %s), relance dans %d s' % (self.proc.returncode, delay), xbmc.LOGWARNING)
        self.proc = None
        self.next_start = time.time() + delay


def run():
    svc = Service()
    player = Player(svc)  # garde une reference : les rappels s'arretent sinon
    a = addon()
    import_old_credentials(a)
    # textes d'etat dans la langue de l'interface (les valeurs par defaut ne se traduisent pas)
    if not a.getSetting('paired'):
        a.setSetting('paired', tr(30107))
    if not a.getSetting('area_name'):
        a.setSetting('area_name', tr(30109))
    svc.apply()
    svc.update_keymap()
    while not svc.waitForAbort(2):
        svc.tick()
    svc.stop()
    del player
