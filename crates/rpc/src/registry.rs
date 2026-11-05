//! Objects are identified by the numeric `rpcObjectId` the host chooses; create calls pass the
//! new object's id and other calls address an existing one (win-rpc §2) [I]. Nothing registers
//! objects in phase 1; this carries the lookup and error rules phase 2 builds on.

use std::collections::HashMap;

use proto::hresult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError {
    pub hresult: u32,
    pub message: String,
}

#[derive(Debug)]
pub struct Registry<T> {
    objects: HashMap<(String, u64), T>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Registry {
            objects: HashMap::new(),
        }
    }
}

impl<T> Registry<T> {
    pub fn create(
        &mut self,
        object_type: &str,
        id: u64,
        object: T,
    ) -> Result<&mut T, RegistryError> {
        use std::collections::hash_map::Entry;
        match self.objects.entry((object_type.to_owned(), id)) {
            Entry::Occupied(_) => Err(RegistryError {
                hresult: hresult::ERROR_ALREADY_EXISTS,
                message: format!("{object_type} already exists"),
            }),
            Entry::Vacant(v) => Ok(v.insert(object)),
        }
    }

    pub fn get_mut(&mut self, object_type: &str, id: u64) -> Result<&mut T, RegistryError> {
        self.objects
            .get_mut(&(object_type.to_owned(), id))
            .ok_or_else(|| RegistryError {
                hresult: hresult::ERROR_NOT_FOUND,
                message: format!("{object_type} not found"),
            })
    }

    pub fn remove(&mut self, object_type: &str, id: u64) -> Result<T, RegistryError> {
        self.objects
            .remove(&(object_type.to_owned(), id))
            .ok_or_else(|| RegistryError {
                hresult: hresult::ERROR_NOT_FOUND,
                message: format!("{object_type} not found"),
            })
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_get_remove() {
        let mut r = Registry::default();
        r.create("MediaStream", 1, "a").unwrap();
        assert_eq!(*r.get_mut("MediaStream", 1).unwrap(), "a");
        assert_eq!(
            r.create("MediaStream", 1, "b").unwrap_err().hresult,
            hresult::ERROR_ALREADY_EXISTS
        );
        assert_eq!(
            r.get_mut("MediaStream", 2).unwrap_err().hresult,
            hresult::ERROR_NOT_FOUND
        );
        // The same id under another object type is a different object.
        r.create("MediaElement", 1, "c").unwrap();
        assert_eq!(r.remove("MediaStream", 1).unwrap(), "a");
        assert_eq!(r.len(), 1);
    }
}
