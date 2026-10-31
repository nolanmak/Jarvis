#!/usr/bin/env python3
"""Guest stdio relay; authorization is performed by the host MCP bridge."""
import os
import socket
import sys
import threading

connection = socket.socket(socket.AF_UNIX)
connection.connect('/tmp/native-mcp.sock')

def write():
    try:
        while data := sys.stdin.buffer.read1(65536):
            connection.sendall(data)
        connection.shutdown(socket.SHUT_WR)
    except OSError:
        os._exit(1)

threading.Thread(target=write, daemon=True).start()
while data := connection.recv(65536):
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
