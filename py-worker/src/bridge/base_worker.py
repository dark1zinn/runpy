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
        self.__clycles = 0
        self.__sock_path = sock

        try:
            self.stream = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.stream.connect(self.__sock_path)
        except Exception as e:
            print(f"Failed to set up socket: {e}")
            sys.exit(1)

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
    
    def send(self, msg_type: str, message: str=None, data=None):
        """Sends a message back to Rust's Control Plane"""
        try:
            if not msg_type: 
                raise ValueError("Message type is required")
            payload = {
                "type": msg_type.upper(),
                "message": message or "",
                "data": data or {}
            }
            self.__send_message(json.dumps(payload).encode())
        except Exception as e:
            print(f"Failed to notify Rust: {e}")
            self.__ok = False
            # Since we can't communicate with Rust, we should terminate the worker to avoid hanging
            sys.exit(1)

    def handle_request(self, request_data: dict) -> dict:
        """Handle incoming requests from Rust. Override this method in your worker subclass."""
        raise NotImplementedError

    def execute(self, payload: dict) -> dict:
        """Include your main logic here, then just return the result as a dict. This is called when an EXECUTE message is received. Upon completion, the result will be sent back to Rust as a DONE message."""
        return None
    
    def __handle_request(self, request_data: dict):
        """Internal method to handle incoming requests and send responses"""
        try:
            match request_data.get("type"):
                case None:
                    self.send("DEBUG", "Received request with no type", {"request": request_data})
                    return
                
                case "TERMINATE":
                    sys.exit(0)

                case "META":
                    if request_data.get("data", {}).get("name"):
                        self.name = request_data.get("data", {}).get("name")
                    self.send("DEBUG", "Received META data", request_data.get('data'))
                    return

                case "EXECUTE":
                    self.__exec_payload = request_data.get("data", None)
                    self.send("DEBUG", "Received EXECUTE request", {"payload": self.__exec_payload})
                    try:
                        result = self.execute(self.__exec_payload)
                        if result is not None:
                            self.send("DONE", "Execution completed", result)
                        else:
                            self.send("DONE", "Execution completed with no result", {})
                        return
                    except Exception as e:
                        self.send("ERROR", f"Execution error: {e}")
                        return

                case "RETRY":
                    self.__clycles += 1
                    self.send("DEBUG", f"Received RETRY request, re-executing (cycle: {self.__clycles})", {})
                    try:
                        result = self.execute(self.__exec_payload)
                        if result is not None:
                            self.send("DONE", f"Execution completed on retry({self.__clycles})", result)
                        else:
                            self.send("DONE", f"Execution completed on retry({self.__clycles}) with no result", {})
                        return
                    except Exception as e:
                        self.send("ERROR", f"Execution error on retry: {e}")
                        return

                case _:
                    self.send("DEBUG", "Received unrecognized request type", {"request": request_data})
                    return
                
        except Exception as e:
            self.send("ERROR", f"Error handling request: {e}")

    def run(self):
        """Public method to safely start the worker"""
        if not self.__ok:
            print("Worker initialization failed, cannot start")
            return
        self.__run()

    def __run(self):
        print(f"Worker listening on {self.__sock_path}")

        while self.__ok:
            try:
                request = self.__recv_message()
                if not request: continue
                # The actual logic happens here
                self.__handle_request(request)
                self.handle_request(request)
            except Exception as e:
                print(f"Error handling request: {e}")
                self.send("ERROR", str(e))
                self.send("DEBUG", "Terminating worker due to error", {})
                self.send("TERMINATE")
                self.__ok = False
                sys.exit(1)