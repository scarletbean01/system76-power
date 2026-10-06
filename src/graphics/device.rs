// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

use std::io;
use sysfs_class::{PciDevice, SysClass};

use super::error::GraphicsDeviceError;

#[derive(Clone)]
pub struct GraphicsDevice {
    pub(super) id: String,
    devid: u16,
    functions: Vec<PciDevice>,
}

impl std::fmt::Debug for GraphicsDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphicsDevice")
            .field("id", &self.id)
            .field("devid", &self.devid)
            .field("functions_count", &self.functions.len())
            .finish()
    }
}

impl GraphicsDevice {
    #[must_use]
    pub fn new(id: String, devid: u16, functions: Vec<PciDevice>) -> Self {
        Self { id, devid, functions }
    }

    #[must_use]
    pub fn exists(&self) -> bool {
        self.functions.iter().any(|func| func.path().exists())
    }

    #[must_use]
    pub const fn device(&self) -> u16 {
        self.devid
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn functions(&self) -> &[PciDevice] {
        &self.functions
    }

    #[must_use]
    pub fn is_driver_bound(&self) -> bool {
        self.functions.iter().any(|func| func.path().exists() && func.driver().is_ok())
    }

    pub unsafe fn unbind(&self) -> Result<(), GraphicsDeviceError> {
        for func in &self.functions {
            if func.path().exists() {
                match func.driver() {
                    Ok(driver) => {
                        log::info!("{}: Unbinding {}", driver.id(), func.id());
                        unsafe {
                            driver.unbind(func).map_err(|why| GraphicsDeviceError::Unbind {
                                driver: driver.id().to_owned(),
                                func: func.id().to_owned(),
                                why,
                            })?;
                        }
                    }
                    Err(why) => match why.kind() {
                        io::ErrorKind::NotFound => (),
                        _ => {
                            return Err(GraphicsDeviceError::PciDriver {
                                device: self.id.clone(),
                                why,
                            })
                        }
                    },
                }
            }
        }
        Ok(())
    }

    pub unsafe fn remove(&self) -> Result<(), GraphicsDeviceError> {
        for func in &self.functions {
            if func.path().exists() {
                match func.driver() {
                    Ok(driver) => {
                        log::error!("{}: in use by {}", func.id(), driver.id());
                        return Err(GraphicsDeviceError::DeviceInUse {
                            func: func.id().to_owned(),
                            driver: driver.id().to_owned(),
                        });
                    }
                    Err(why) => match why.kind() {
                        io::ErrorKind::NotFound => {
                            log::info!("{}: Removing", func.id());
                            unsafe {
                                func.remove().map_err(|why| GraphicsDeviceError::Remove {
                                    device: self.id.clone(),
                                    why,
                                })?;
                            }
                        }
                        _ => {
                            return Err(GraphicsDeviceError::PciDriver {
                                device: self.id.clone(),
                                why,
                            })
                        }
                    },
                }
            } else {
                log::warn!("{}: Already removed", func.id());
            }
        }
        Ok(())
    }
}
