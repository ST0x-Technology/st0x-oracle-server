variable "do_token" {
  description = "DigitalOcean API token"
  type        = string
  sensitive   = true
}

variable "ssh_key_name" {
  description = "Name of the SSH key in DigitalOcean to add to the droplet"
  type        = string
  default     = "st0x-op"
}

variable "region" {
  description = "DigitalOcean region"
  type        = string
  default     = "nyc3"
}

# Oracle server: single WebSocket to st0x-pricing (post-RAI-360), one
# REST poll loop to Alpaca's broker /calendar endpoint, and an axum
# HTTP server signing per-request SignedContextV1 frames. Cheap; 1 GB
# is comfortable.
variable "droplet_size" {
  description = "Droplet size slug"
  type        = string
  default     = "s-1vcpu-1gb"
}

variable "volume_size_gb" {
  description = "Block storage volume size in GB"
  type        = number
  default     = 5
}
