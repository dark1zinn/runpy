from bridge.base_worker import BaseWorker
import sys

class ScraperWorker(BaseWorker):
    def handle_request(self, request_data):
        # Business logic is isolated here
        print(f"Received HTML: {request_data['html'][:50]}...")  # Just a preview
        
        return {
            "status": "success",
            "title": "Hello from Python!",
            "links_count": 1
        }

if __name__ == "__main__":
    # Rust will pass the socket path as the first argument
    worker = ScraperWorker(sys.argv[1])
    worker.run()