import socket
import json
import os
import sys
import struct

from typing import Type

def RunScript(worker_class: Type[BaseWorker]):
    """A simple wrapper to safely instantiate and run a worker class"""
    try:
        if len(sys.argv) < 2:
            print("Error: Socket path argument required")
            sys.exit(1)
        
        socket_path = sys.argv[1]
        
        # Validate that worker_class is a BaseWorker subclass
        if not issubclass(worker_class, BaseWorker):
            raise TypeError(f"{worker_class.__name__} must inherit from BaseWorker")
        
        # Instantiate and run
        worker = worker_class(socket_path)
        worker.run()
        
    except TypeError as e:
        print(f"Configuration error: {e}")
        sys.exit(1)
    except Exception as e:
        print(f"Worker initialization failed: {e}")
        sys.exit(1)

class BaseWorker:
    def __init__(self, sock):
        self.__ok = False
        self._sock_path = sock

        try:
            # if os.path.exists(self._sock_path):
            #     os.remove(self._sock_path)
            self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.stream.connect(self._sock_path)
        except Exception as e:
            print(f"Failed to set up socket: {e}")
            exit(1)

        self.__ok = True
        self.send("READY", "Worker is ready to receive requests")

    def __send_message(self, data: bytes):
        """Send a length-prefixed message"""
        try:
            # Pack size as little-endian u64 (8 bytes)
            size = struct.pack('<Q', len(data))
            self.stream.sendall(size + data)
        except Exception as e:
            print(f"Failed to send message: {e}")
            self.__ok = False
    
    def __recv_message(self) -> dict:
        """Receive a length-prefixed message"""
        try:
            # Read 8 bytes for the size
            size_data = self.stream.recv(8)
            if not size_data:
                return None
            
            size = struct.unpack('<Q', size_data)[0]
            
            # Read exactly `size` bytes
            data = b""
            while len(data) < size:
                chunk = self.stream.recv(size - len(data))
                if not chunk:
                    raise ConnectionError("Connection closed unexpectedly")
                data += chunk
            
            return json.loads(data.decode())
        except Exception as e:
            print(f"Error receiving message: {e}")
            return None
    
    def send(self, msg_type: str, message: str, data=None):
        """Sends a message back to Rust's Control Plane"""
        payload = {
            "type": msg_type.upper(),
            "message": message,
            "data": data or {}
        }
        try:
            self.__send_message(json.dumps(payload).encode())
        except Exception as e:
            print(f"Failed to notify Rust: {e}")
            self.__ok = False

    def handle_request(self, request_data: dict) -> dict:
        """Override this in your script!"""
        raise NotImplementedError

    def run(self):
        """Public method to safely start the worker"""
        if not self.__ok:
            print("Worker initialization failed, cannot start")
            return
        self.__run()

    def __run(self):
        print(f"Worker listening on {self._sock_path}")

        while self.__ok:
            try:
                request = self.__recv_message()
                if not request: continue
                # The actual logic happens here
                self.handle_request(request)
            except Exception as e:
                print(f"Error handling request: {e}")
                self.send("ERROR", str(e))
                self.__ok = False
                exit(1)