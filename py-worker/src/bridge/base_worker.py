import socket
import json
import os
import sys

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
            if os.path.exists(self._sock_path):
                os.remove(self._sock_path)
            self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.server.connect(self._sock_path)
        except Exception as e:
            print(f"Failed to set up socket: {e}")
            exit(1)

        self.__ok = True
        self.notify_rust("READY", "Worker is ready to receive requests")

    def notify_rust(self, msg_type: str, message: str, data=None):
        """Sends a message back to Rust's Control Plane"""
        payload = {
            "type": msg_type.upper(),
            "message": message,
            "data": data or {}
        }
        try:
            self.server.sendall(json.dumps(payload).encode())
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
        server = self.server
        print(f"Worker listening on {self._sock_path}")

        while self.__ok:
            try:
                conn, _ = server.accept()
                with conn:
                    raw_data = conn.recv(1024 * 10).decode() # Adjust buffer as needed
                    if not raw_data: continue
                    
                    request = json.loads(raw_data)
                    # The actual logic happens here
                    self.handle_request(request)
            except Exception as e:
                print(f"Error handling request: {e}")
                self.notify_rust("ERROR", str(e))
                self.__ok = False
                exit(1)