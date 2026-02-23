from runpyrs import Worker, RunScript

class MyWorker(Worker):

    def handle_request(self, request_data: dict):
        match request_data.get("type"):
            case _:
                 self.send("DEBUG", "Received request", {"request": request_data})
                 return

    def execute(self, payload: dict) -> dict:
        # Business logic is isolated here
        try:
            self.send("INFO", "Starting parse operation", {"message": f"Received HTML: {payload}"})
            # logic here...
            return {
                "status": "success",
                "title": "Hello from Python!",
                "links_count": 1
            }
        except Exception as e:
            # It's ok to raise exceptions here, they will be caught and sent back to Rust as ERROR messages, thus terminating the worker gracefully.
            raise RuntimeError(f"Error during execution: {e}")

if __name__ == "__main__":
    # The RunScript function abstracts away the worker initialization and execution
    RunScript(MyWorker)