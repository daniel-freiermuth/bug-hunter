import signal

from .cli import main

signal.signal(signal.SIGPIPE, signal.SIG_DFL)
main()
