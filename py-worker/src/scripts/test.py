from bridge.base_worker import BaseWorker, RunScript

class TestWorker(BaseWorker):
    def handle_request(self, request_data):
        # Business logic is isolated here
        try:
            self.send("INFO", "Starting parse operation", {"message": f"Received HTML: {request_data}"})
            # logic here...
            self.send("DONE", "Scraping complete", {
                "status": "success",
                "title": "Hello from Python!",
                "links_count": 1
            })
            return
        except Exception as e:
            self.send("ERROR", str(e))
            return

if __name__ == "__main__":
    # The RunScript function abstracts away the worker initialization and execution
    RunScript(TestWorker)