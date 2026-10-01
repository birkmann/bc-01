//! Default energy calibration, fitted on 3000 library tracks (`bc-analysis-tool calibrate-energy --emit-rust`).

use crate::energy::EnergyCalibration;

pub fn default_calibration() -> EnergyCalibration {
    EnergyCalibration {
        mean: [-9.364232, 0.047142684, 8.920113],
        sd: [2.5249457, 0.01155365, 0.79222476],
        weights: [0.55, 0.25, 0.2],
        quantiles: vec![-6.4459987, -1.2652876, -0.851143, -0.6320681, -0.47388107, -0.358378, -0.2267442, -0.14575368, -0.04839775, 0.040689185, 0.10892394, 0.18194656, 0.2680191, 0.34087983, 0.40623462, 0.4777693, 0.5600787, 0.64665425, 0.7685733, 0.95502996, 2.1052368],
    }
}

