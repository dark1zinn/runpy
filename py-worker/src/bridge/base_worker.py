import socket
import json
import os

class BaseWorker:
    def __init__(self, socket_path):
        self.socket_path = socket_path

    def handle_request(self, request_data: dict) -> dict:
        """Override this in your script!"""
        raise NotImplementedError

    def run(self):
        if os.path.exists(self.socket_path):
            os.remove(self.socket_path)

        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as server:
            server.bind(self.socket_path)
            server.listen(1)
            print(f"Worker listening on {self.socket_path}")

            while True:
                conn, _ = server.accept()
                with conn:
                    raw_data = conn.recv(1024 * 10).decode() # Adjust buffer as needed
                    if not raw_data: continue
                    
                    request = json.loads(raw_data)
                    # The actual logic happens here
                    response = self.handle_request(request)
                    
                    conn.sendall(json.dumps(response).encode())