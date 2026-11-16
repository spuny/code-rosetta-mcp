terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
  backend "s3" {
    bucket = "state"
    key    = "network/terraform.tfstate"
  }
}

provider "aws" {
  region = "eu-west-1"
}

provider "aws" {
  alias  = "west"
  region = "us-west-2"
}

variable "name" {
  type        = string
  default     = "demo"
  description = "Name prefix"
}

variable "db_password" {
  type    = string
  default = "hunter2"
}

variable "disks" {
  type = list(object({
    name = string
  }))
  default = []
}

locals {
  prefix = "${var.name}-app"
  tags   = { Name = local.prefix }
  config = yamldecode(file("${path.module}/config/app.yaml"))
  ca     = file("../shared/ca.pem")
  pairs  = [for k, v in var.disks : "${k}=${v.name}"]
  api_token = "abc"
}

data "aws_ami" "base" {
  most_recent = true
  owners      = ["amazon"]
}

resource "aws_instance" "web" {
  ami           = data.aws_ami.base.id
  instance_type = var.instance_type
  tags          = local.tags
  user_data     = templatefile("${path.module}/files/boot.sh.tpl", { name = var.name })
  depends_on    = [aws_security_group.web]

  dynamic "ebs_block_device" {
    for_each = var.disks
    content {
      device_name = ebs_block_device.value.name
      volume_size = aws_ebs_volume.shared.size
    }
  }

  lifecycle {
    ignore_changes = [tags]
  }
}

resource "aws_security_group" "web" {
  name = "${local.prefix}-sg"
}

module "vpc" {
  source = "./modules/vpc"
  cidr   = var.name
  count  = 1
}

module "registry" {
  source  = "terraform-aws-modules/s3-bucket/aws"
  version = "4.0.0"
  bucket  = var.name
}

output "instance_id" {
  description = "Web instance"
  value       = aws_instance.web[0].id
}

output "vpc" {
  value = module.vpc.id
}
