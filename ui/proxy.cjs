module.exports = {
  '/v1/admin/**': { target: `http://127.0.0.1:${process.env.ERP4US_DIAGNOSTICS_PORT || '8091'}` },
  '/v1/sales/**': { target: `http://127.0.0.1:${process.env.ERP4US_SALES_PORT || '8092'}` },
  '/v1/inventory/**': { target: `http://127.0.0.1:${process.env.ERP4US_INVENTORY_PORT || '8093'}` },
  '/v1/procurement/**': { target: `http://127.0.0.1:${process.env.ERP4US_PROCUREMENT_PORT || '8094'}` },
  '/v1/manufacturing/**': { target: `http://127.0.0.1:${process.env.ERP4US_MANUFACTURING_PORT || '8090'}` },
  '/v1/**': { target: `http://127.0.0.1:${process.env.ERP4US_PORT || '8080'}` }
};
