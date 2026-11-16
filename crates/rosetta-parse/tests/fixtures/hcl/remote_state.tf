data "terraform_remote_state" "network" {
  backend = "s3"
  config = {
    bucket = "state"
    key    = "network/terraform.tfstate"
  }
}

data "terraform_remote_state" "dynamic" {
  backend = "s3"
  config = {
    bucket = "state"
    key    = "${var.env}/terraform.tfstate"
  }
}

resource "aws_subnet" "a" {
  vpc_id = data.terraform_remote_state.network.outputs.vpc_id
  cidr   = data.terraform_remote_state.network.outputs["cidr"]
  zone   = data.terraform_remote_state.dynamic.outputs.zone
  other  = data.terraform_remote_state.missing.outputs.x
}
