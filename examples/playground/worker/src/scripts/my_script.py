from runpyrs import Worker, RunScript

class MyWorker(Worker):

    def handle_request(self, request_data: dict):
        """Handle non-internal requests using the new HTTP-like protocol."""
        method = request_data.get("method")
        match method:
            case _:
                self.send("LOG", message="Received request", body={"request": request_data}, headers={"X-Log-Level": "debug"})
                return

    def execute(self, payload: dict) -> dict:
        """Business logic is isolated here."""
        try:
            self.send("LOG", message="Starting parse operation", body={"payload": payload}, headers={"X-Log-Level": "info"})
            # logic here...
            return {
                "status": "success",
                "title": "Hello from Python!",
                "links_count": 1
            }
        except Exception as e:
            # It's ok to raise exceptions here, they will be caught and sent back 
            # to Rust as ERROR messages, thus terminating the worker gracefully.
            raise RuntimeError(f"Error during execution: {e}")

if __name__ == "__main__":
    # The RunScript function abstracts away the worker initialization and execution
    RunScript(MyWorker)