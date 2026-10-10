# -*- coding: utf-8 -*-
"""Service Hue Ambi : lance le moteur Rust selon les reglages et le relance s'il s'arrete."""
import subprocess
import threading
import time

import xbmc

from hueambi import CONF, REQUIRED, addon, binary, env, import_old_credentials, log, tr, values, write_conf


class Service(xbmc.Monitor):
    def __init__(self):
        super().__init__()
        self.proc = None
        self.current = False  # configuration du moteur en cours, None si inactif, False au depart
        self.fails = 0
        self.started = 0.0
        self.next_start = 0.0

    def onSettingsChanged(self):
        self.apply()

    def apply(self):
        a = addon()
        v = values(a)
        enabled = a.getSettingBool('enabled')
        want = v if enabled and all(v[k] for k in REQUIRED) else None
        if want == self.current and (want is None or self.proc is not None):
            return
        self.stop()
        write_conf(v)  # sert aussi aux actions (liste des zones)
        self.current = want
        if want is None:
            log('inactif : %s' % ('desactive' if not enabled else 'appairage ou zone manquant'))
            return
        self.fails = 0
        self.start()

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

    def tick(self):
        if self.current is None:
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


if __name__ == '__main__':
    svc = Service()
    a = addon()
    import_old_credentials(a)
    # textes d'etat dans la langue de l'interface (les valeurs par defaut ne se traduisent pas)
    if not a.getSetting('paired'):
        a.setSetting('paired', tr(30107))
    if not a.getSetting('area_name'):
        a.setSetting('area_name', tr(30109))
    svc.apply()
    while not svc.waitForAbort(2):
        svc.tick()
    svc.stop()
