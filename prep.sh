#!/bin/bash
set -euxo pipefail

echo "Building sol artifacts..."
(cd lib/rain.math.float && forge build)

echo "Setup complete!"
