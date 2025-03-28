use feos_core::{Contributions, EosError, EosResult, EosUnit, SolverOptions, State, StateBuilder};
use feos_dft::{
    Axis, ConvolverFFT, DFTProfile, DFTSolver, DFTSpecification, Geometry, Grid,
    HelmholtzEnergyFunctional, WeightFunctionInfo, DFT,
};
use ndarray::prelude::*;
use quantity::{QuantityArray2, QuantityScalar, SIArray1};
use std::sync::Arc;

pub enum MicelleInitialization {
    ExternalPotential(f64, f64),
    Density(QuantityArray2),
}

impl MicelleInitialization {
    fn density(&self) -> Option<&QuantityArray2> {
        match self {
            Self::ExternalPotential(_, _) => None,
            Self::Density(density) => Some(density),
        }
    }
}

pub enum MicelleSpecification {
    ChemicalPotential,
    Size {
        delta_n_surfactant: f64,
        pressure: SINumber,
    },
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> DFTSpecification<U, Ix1, F>
    for MicelleSpecification
{
    fn calculate_bulk_density(
        &self,
        profile: &DFTProfile<U, Ix1, F>,
        bulk_density: &Array1<f64>,
        z: &Array1<f64>,
    ) -> EosResult<Array1<f64>> {
        Ok(match self {
            Self::ChemicalPotential => bulk_density.clone(),
            Self::Size {
                delta_n_surfactant,
                pressure,
            } => {
                let rho_s_bulk = bulk_density[1];
                let rho_w_bulk = bulk_density[0];
                let volume = SIUnit::reference_volume();
                let moles = arr1(&[rho_w_bulk, rho_s_bulk]) * SIUnit::reference_density() * volume;
                let bulk = State::new_nvt(&profile.dft, profile.temperature, volume, &moles)?;
                let f_bulk = bulk.helmholtz_energy(Contributions::Total) / bulk.volume;
                let mu_bulk = bulk.chemical_potential(Contributions::Total);
                let mu_s_bulk = mu_bulk.get(1);
                let mu_w_bulk = mu_bulk.get(0);
                let n_s_bulk =
                    (rho_s_bulk * profile.volume()).to_reduced(SIUnit::reference_moles())?;
                let mut spec = (delta_n_surfactant + n_s_bulk) / z;
                spec[0] = ((pressure + f_bulk - rho_s_bulk * mu_s_bulk) / mu_w_bulk)
                    .to_reduced(SIUnit::reference_density())?;
                spec
            }
        })
    }
}

pub struct MicelleProfile<U: EosUnit, F: HelmholtzEnergyFunctional> {
    pub profile: DFTProfile<U, Ix1, F>,
    pub delta_omega: Option<SINumber>,
    pub delta_n: Option<SIArray1>,
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> Clone for MicelleProfile<U, F> {
    fn clone(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            delta_omega: self.delta_omega,
            delta_n: self.delta_n.clone(),
        }
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> MicelleProfile<U, F> {
    pub fn solve_inplace(&mut self, solver: Option<&DFTSolver>, debug: bool) -> EosResult<()> {
        self.profile.solve(solver, debug)?;
        self.post_process()
    }

    pub fn solve(mut self, solver: Option<&DFTSolver>) -> EosResult<Self> {
        self.solve_inplace(solver, false)?;
        Ok(self)
    }

    pub fn solve_micelle_inplace(
        &mut self,
        solver1: Option<&DFTSolver>,
        solver2: Option<&DFTSolver>,
        debug: bool,
    ) -> EosResult<()> {
        self.profile.solve(solver1, true)?;
        self.profile.external_potential.fill(0.0);
        self.profile.solve(solver2, debug)?;
        self.post_process()
    }

    pub fn solve_micelle(
        mut self,
        solver1: Option<&DFTSolver>,
        solver2: Option<&DFTSolver>,
    ) -> EosResult<Self> {
        self.solve_micelle_inplace(solver1, solver2, false)?;
        Ok(self)
    }

    fn post_process(&mut self) -> EosResult<()> {
        // calculate excess grand potential
        self.delta_omega = Some(self.profile.integrate(
            &(self.profile.dft.grand_potential_density(
                self.profile.temperature,
                &self.profile.density,
                &self.profile.convolver,
            )? + self.profile.bulk.pressure(Contributions::Total)),
        ));

        // calculate excess particles
        self.delta_n =
            Some(self.profile.moles() - &self.profile.bulk.partial_density * self.profile.volume());

        Ok(())
    }
}

impl<U: EosUnit + 'static, F: HelmholtzEnergyFunctional> MicelleProfile<U, F> {
    fn new(
        bulk: &State<U, F>,
        axis: Axis,
        initialization: MicelleInitialization,
        specification: MicelleSpecification,
    ) -> EosResult<Self> {
        let dft = &bulk.eos;

        // calculate external potential
        let t = bulk
            .temperature
            .to_reduced(SIUnit::reference_temperature())?;
        let mut external_potential = Array2::zeros((dft.component_index().len(), axis.grid.len()));
        if let MicelleInitialization::ExternalPotential(peak, width) = initialization {
            external_potential.index_axis_mut(Axis(0), 0).assign(
                &axis
                    .grid
                    .mapv(|r| peak * (-0.5 * r * r / (width * width)).exp()),
            );
        }

        // initialize convolver
        let grid = match axis.geometry {
            Geometry::Spherical => Grid::Spherical(axis),
            Geometry::Cylindrical => Grid::Polar(axis),
            _ => unreachable!(),
        };
        let contributions = dft.contributions();
        let weight_functions: Vec<WeightFunctionInfo<f64>> = contributions
            .iter()
            .map(|c| c.weight_functions(t))
            .collect();
        let convolver = ConvolverFFT::plan(&grid, &weight_functions, Some(1));

        // create profile
        let mut profile = DFTProfile::new(
            grid,
            convolver,
            bulk,
            Some(external_potential),
            initialization.density(),
        )?;

        // specify specification
        profile.specification = Arc::new(specification);

        Ok(Self {
            profile,
            delta_omega: None,
            delta_n: None,
        })
    }

    pub fn new_spherical(
        bulk: &State<U, F>,
        n_grid: usize,
        width: SINumber,
        initialization: MicelleInitialization,
        specification: MicelleSpecification,
    ) -> EosResult<Self> {
        Self::new(
            bulk,
            Axis::new_spherical(n_grid, width)?,
            initialization,
            specification,
        )
    }

    pub fn new_cylindrical(
        bulk: &State<U, F>,
        n_grid: usize,
        width: SINumber,
        initialization: MicelleInitialization,
        specification: MicelleSpecification,
    ) -> EosResult<Self> {
        Self::new(
            bulk,
            Axis::new_polar(n_grid, width)?,
            initialization,
            specification,
        )
    }

    pub fn update_specification(&self, specification: MicelleSpecification) -> Self {
        let mut profile = self.clone();
        profile.profile.specification = Arc::new(specification);
        profile.delta_omega = None;
        profile.delta_n = None;
        profile
    }
}

const MAX_ITER_MICELLE: usize = 50;
const TOL_MICELLE: f64 = 1e-5;

impl<U: EosUnit + 'static, F: HelmholtzEnergyFunctional> MicelleProfile<U, F> {
    pub fn critical_micelle(
        mut self,
        solver: Option<&DFTSolver>,
        options: SolverOptions,
    ) -> EosResult<Self> {
        let n_grid = self.profile.r().len();
        let temperature = self.profile.bulk.temperature;
        let t = temperature.to_reduced(SIUnit::reference_temperature())?;
        let pressure = self.profile.bulk.pressure(Contributions::Total);
        let eos = self.profile.bulk.eos.clone();
        let indices = self.profile.bulk.eos.component_index().into_owned();
        self.profile.specification = Arc::new(MicelleSpecification::ChemicalPotential);

        for _ in 0..options.max_iter.unwrap_or(MAX_ITER_MICELLE) {
            // check for convergence
            if self
                .delta_omega
                .unwrap()
                .to_reduced(SIUnit::reference_energy())?
                .abs()
                < options.tol.unwrap_or(TOL_MICELLE) * t
            {
                return Ok(self);
            }

            let bulk = &mut self.profile.bulk;
            let mut x = bulk.molefracs[1];

            // Calculate Newton step
            let delta_n = self.delta_n.as_ref().unwrap();
            let dp_drho = bulk.dp_dni(Contributions::Total) * bulk.volume;
            let dmu_drho = bulk.dmu_dni(Contributions::Total) * bulk.volume;
            let p_term = (dp_drho.get(1) - dp_drho.get(0))
                / (dp_drho.get(1) * x + dp_drho.get(0) * (1.0 - x));
            let a = delta_n.get(1) * dmu_drho.get((1, 1)) + delta_n.get(0) * dmu_drho.get((1, 0));
            let b = delta_n.get(1) * dmu_drho.get((0, 1)) + delta_n.get(0) * dmu_drho.get((0, 0));
            let domega_dx = -(((a - b) * x + b) * p_term + a - b) * bulk.density;
            x -= self.delta_omega.unwrap().to_reduced(domega_dx)?;

            // udpate bulk and chemical potential
            *bulk = StateBuilder::new(&eos)
                .temperature(temperature)
                .pressure(pressure)
                .molefracs(&arr1(&[1.0 - x, x]))
                .build()?;

            // update density profile
            for (i, &j) in indices.iter().enumerate() {
                let rho_bulk = self.profile.density.get((i, n_grid - 1));
                for k in 0..n_grid {
                    let rho_old = self.profile.density.get((i, k));
                    self.profile
                        .density
                        .try_set((i, k), rho_old + bulk.partial_density.get(j) - rho_bulk)?;
                }
            }

            // solve profile
            self = self.solve(solver)?;
        }

        Err(EosError::NotConverged(
            "MicelleProfile::criticelle_micelle".into(),
        ))
    }
}
