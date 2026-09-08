import os
from pathlib import Path
import socket
import sys
import tempfile
import unittest

from listener_owner import owns_listener


class ListenerOwnerTests(unittest.TestCase):
    def test_other_process_listener_is_not_accepted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '123/fd').mkdir(parents=True)
            (root / '123/net').mkdir()
            (root / '123/fd/4').symlink_to('socket:[41]')
            (root / '123/net/tcp').write_text(
                'header\n0: 0100007F:1F90 00000000:0000 0A 0 0 0 0 0 42\n')
            self.assertFalse(owns_listener(123, 8080, root))
            (root / '123/fd/5').symlink_to('socket:[42]')
            self.assertTrue(owns_listener(123, 8080, root))
            self.assertFalse(owns_listener(123, 8081, root))
            self.assertFalse(owns_listener(124, 8080, root))

    @unittest.skipUnless(sys.platform == 'linux', 'requires Linux procfs')
    def test_real_listener(self):
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            port = listener.getsockname()[1]
            self.assertTrue(owns_listener(os.getpid(), port))
            self.assertFalse(owns_listener(os.getppid(), port))
        self.assertFalse(owns_listener(os.getpid(), port))
