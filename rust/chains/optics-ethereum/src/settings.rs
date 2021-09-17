use color_eyre::{Report, Result};
use ethers::prelude::{Address, Middleware};
use std::convert::TryFrom;

use crate::*;

use optics_core::{db::DB, ConnectionManager, Home, Replica, Signers};